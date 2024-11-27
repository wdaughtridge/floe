use std::ops::Deref;

#[derive(Debug)]
pub struct Client {
    pub id: ClientId,
    pub rtc: str0m::Rtc,
    pub cid: Option<str0m::channel::ChannelId>,
    pub tracks_in: Vec<TrackInEntry>,
    pub tracks_out: Vec<TrackOut>,
    pub chosen_rid: Option<str0m::media::Rid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientId(u64);

impl Deref for ClientId {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct TrackIn {
    origin: ClientId,
    mid: str0m::media::Mid,
    kind: str0m::media::MediaKind,
}

#[derive(Debug)]
pub struct TrackInEntry {
    pub id: std::sync::Arc<TrackIn>,
    last_keyframe_request: Option<std::time::Instant>,
}

#[derive(Debug)]
pub struct TrackOut {
    track_in: std::sync::Weak<TrackIn>,
    state: TrackOutState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum TrackOutState {
    ToOpen,
    Negotiating(str0m::media::Mid),
    Open(str0m::media::Mid),
}

impl TrackOut {
    fn mid(&self) -> Option<str0m::media::Mid> {
        match self.state {
            TrackOutState::ToOpen => None,
            TrackOutState::Negotiating(m) | TrackOutState::Open(m) => Some(m),
        }
    }
}

impl Client {
    pub fn new(rtc: str0m::Rtc) -> Client {
        static ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let next_id = ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Client {
            id: ClientId(next_id),
            rtc,
            cid: None,
            tracks_in: vec![],
            tracks_out: vec![],
            chosen_rid: None,
        }
    }

    pub fn accepts(&self, input: &str0m::Input) -> bool {
        self.rtc.accepts(input)
    }

    pub fn handle_new_candidate(&mut self, candidate: str0m::Candidate) {
        if !self.rtc.is_alive() {
            return;
        }

        self.rtc.add_remote_candidate(candidate);
    }

    pub fn handle_input(&mut self, input: str0m::Input) {
        if !self.rtc.is_alive() {
            return;
        }

        if let Err(e) = self.rtc.handle_input(input) {
            self.rtc.disconnect();
            panic!("Client ({}) disconnected: {:?}", *self.id, e);
        }
    }

    pub async fn poll_output(&mut self, socket: &tokio::net::UdpSocket) -> Propagated {
        if !self.rtc.is_alive() {
            return Propagated::Noop;
        }

        // Incoming tracks from other clients cause new entries in track_out that
        // need SDP negotiation with the remote peer.
        // if self.negotiate_if_needed().await {
        //     return Propagated::Noop;
        // }

        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output, socket).await,
            Err(e) => {
                self.rtc.disconnect();
                panic!("Client ({}) poll_output failed: {:?}", *self.id, e);
            }
        }
    }

    async fn handle_output(
        &mut self,
        output: str0m::Output,
        socket: &tokio::net::UdpSocket,
    ) -> Propagated {
        match output {
            str0m::Output::Transmit(transmit) => {
                socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await
                    .expect("sending UDP data");
                Propagated::Noop
            }
            str0m::Output::Timeout(t) => Propagated::Timeout(t),
            str0m::Output::Event(e) => match e {
                str0m::Event::IceConnectionStateChange(v) => {
                    if v == str0m::IceConnectionState::Disconnected {
                        self.rtc.disconnect();
                    }
                    Propagated::Noop
                }
                str0m::Event::MediaAdded(e) => self.handle_media_added(e.mid, e.kind).await,
                str0m::Event::MediaData(data) => self.handle_media_data_in(data).await,
                str0m::Event::KeyframeRequest(req) => self.handle_incoming_keyframe_req(req).await,
                str0m::Event::ChannelOpen(cid, _) => {
                    self.cid = Some(cid);
                    Propagated::Noop
                }
                str0m::Event::MediaIngressStats(_data) => Propagated::Noop,
                str0m::Event::MediaEgressStats(_data) => Propagated::Noop,
                str0m::Event::PeerStats(_data) => Propagated::Noop,
                _ => Propagated::Noop,
            },
        }
    }

    async fn handle_media_added(
        &mut self,
        mid: str0m::media::Mid,
        kind: str0m::media::MediaKind,
    ) -> Propagated {
        let track_in = TrackInEntry {
            id: std::sync::Arc::new(TrackIn {
                origin: self.id,
                mid,
                kind,
            }),
            last_keyframe_request: None,
        };

        let weak = std::sync::Arc::downgrade(&track_in.id);
        self.tracks_in.push(track_in);

        Propagated::TrackOpen(self.id, weak)
    }

    async fn handle_media_data_in(&mut self, data: str0m::media::MediaData) -> Propagated {
        if !data.contiguous {
            self.request_keyframe_throttled(
                data.mid,
                data.rid,
                str0m::media::KeyframeRequestKind::Fir,
            )
            .await;
        }

        Propagated::MediaData(self.id, data)
    }

    async fn request_keyframe_throttled(
        &mut self,
        mid: str0m::media::Mid,
        rid: Option<str0m::media::Rid>,
        kind: str0m::media::KeyframeRequestKind,
    ) {
        let Some(mut writer) = self.rtc.writer(mid) else {
            return;
        };

        let Some(track_entry) = self.tracks_in.iter_mut().find(|t| t.id.mid == mid) else {
            return;
        };

        if track_entry
            .last_keyframe_request
            .map(|t| t.elapsed() < std::time::Duration::from_secs(1))
            .unwrap_or(false)
        {
            return;
        }

        _ = writer.request_keyframe(rid, kind);

        track_entry.last_keyframe_request = Some(std::time::Instant::now());
    }

    async fn handle_incoming_keyframe_req(
        &self,
        mut req: str0m::media::KeyframeRequest,
    ) -> Propagated {
        let Some(track_out) = self.tracks_out.iter().find(|t| t.mid() == Some(req.mid)) else {
            return Propagated::Noop;
        };

        let Some(track_in) = track_out.track_in.upgrade() else {
            return Propagated::Noop;
        };

        req.rid = self.chosen_rid;

        Propagated::KeyframeRequest(self.id, req, track_in.origin, track_in.mid)
    }

    pub async fn handle_track_open(&mut self, track_in: std::sync::Weak<TrackIn>) {
        let track_out = TrackOut {
            track_in,
            state: TrackOutState::ToOpen,
        };
        self.tracks_out.push(track_out);
    }

    pub async fn handle_media_data_out(&mut self, data: &str0m::media::MediaData) {
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                o.track_in
                    .upgrade()
                    .filter(|i| {
                        if (i.kind == str0m::media::MediaKind::Video
                            && data.time.frequency().get() == 48_000)
                            || (i.kind == str0m::media::MediaKind::Audio
                                && data.time.frequency().get() == 90_000)
                        {
                            true
                        } else {
                            false
                        }
                    })
                    .is_some()
            })
            .and_then(|o| Some(o.track_in.upgrade().unwrap().mid))
        else {
            return;
        };

        if data.rid.is_some() && data.rid != Some("h".into()) {
            return;
        }

        if self.chosen_rid != data.rid {
            self.chosen_rid = data.rid;
        }

        let writer = self.rtc.writer(mid).expect("rtc writer");

        let pt = writer.match_params(data.params).expect("payload type");

        writer
            .write(pt, data.network_time, data.time, data.data.clone())
            .expect("media data write");
    }

    pub async fn handle_keyframe_request(
        &mut self,
        req: str0m::media::KeyframeRequest,
        mid_in: str0m::media::Mid,
    ) {
        let has_incoming_track = self.tracks_in.iter().any(|i| i.id.mid == mid_in);

        if !has_incoming_track {
            return;
        }

        let Some(mut writer) = self.rtc.writer(mid_in) else {
            return;
        };

        writer
            .request_keyframe(req.rid, req.kind)
            .expect("request keyframe");
    }
}

/// Events propagated between client.
#[allow(clippy::large_enum_variant)]
pub enum Propagated {
    /// When we have nothing to propagate.
    Noop,

    /// Poll client has reached timeout.
    Timeout(std::time::Instant),

    /// A new incoming track opened.
    TrackOpen(ClientId, std::sync::Weak<TrackIn>),

    /// Data to be propagated from one client to another.
    MediaData(ClientId, str0m::media::MediaData),

    /// A keyframe request from one client to the source.
    KeyframeRequest(
        ClientId,
        str0m::media::KeyframeRequest,
        ClientId,
        str0m::media::Mid,
    ),
}

impl Propagated {
    /// Get client id, if the propagated event has a client id.
    pub fn client_id(&self) -> Option<ClientId> {
        match self {
            Propagated::TrackOpen(c, _)
            | Propagated::MediaData(c, _)
            | Propagated::KeyframeRequest(c, _, _, _) => Some(*c),
            _ => None,
        }
    }
}
