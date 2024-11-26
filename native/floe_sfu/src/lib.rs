use rustler::Resource;
use std::ops::Deref;

static TOKIO: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
    // init tokio
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .build()
        .expect("starting tokio")
});

pub fn spawn<T>(task: T) -> tokio::task::JoinHandle<T::Output>
where
    T: std::future::Future + Send + 'static,
    T::Output: Send + 'static,
{
    // wrapper for tokio spawn
    TOKIO.spawn(task)
}

rustler::atoms! {
    ok,     // :ok
    error,  // :error
}

#[derive(PartialEq, Debug)]
enum ClientType {
    Whip,
    Whep,
}

struct SdpHandshake(
    ClientType,
    str0m::change::SdpOffer,
    tokio::sync::oneshot::Sender<str0m::change::SdpAnswer>,
);

struct Link(tokio::sync::mpsc::Sender<SdpHandshake>);

// this is a link back to the main loop for a particular
// stream. this is passed back to the BEAM such that any
// process with the reference to this can add new WHIP
// or WHEP clients.
impl Link {
    fn new(tx: tokio::sync::mpsc::Sender<SdpHandshake>) -> rustler::ResourceArc<Link> {
        rustler::ResourceArc::new(Link(tx))
    }
}

impl Resource for Link {}

// entry point probably started by a WHIP client
#[rustler::nif]
fn start_link() -> (rustler::Atom, rustler::ResourceArc<Link>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<SdpHandshake>(16);

    spawn(main_loop(rx));

    let link = Link::new(tx);

    // link is passed back to BEAM to register in :ets table
    (ok(), link)
}

#[rustler::nif]
fn put_new_whip_client(
    sdp_offer: String,
    link: rustler::ResourceArc<Link>,
) -> (rustler::Atom, String) {
    put_new_client(ClientType::Whip, sdp_offer, link)
}

#[rustler::nif]
fn put_new_whep_client(
    sdp_offer: String,
    link: rustler::ResourceArc<Link>,
) -> (rustler::Atom, String) {
    put_new_client(ClientType::Whep, sdp_offer, link)
}

fn put_new_client(
    client_type: ClientType,
    sdp_offer: String,
    link: rustler::ResourceArc<Link>,
) -> (rustler::Atom, String) {
    // decode offer
    let sdp_offer =
        str0m::change::SdpOffer::from_sdp_string(&sdp_offer).expect("decoding sdp offer");

    // how we get the answer back from the tokio task
    let (tx, rx) = tokio::sync::oneshot::channel::<str0m::change::SdpAnswer>();

    // start an event loop for this client to receive data
    spawn(async move {
        link.0
            .send(SdpHandshake(client_type, sdp_offer, tx))
            .await
            .expect("sending to main loop");
    });

    // get the answer from the spawned task
    let sdp_answer = rx.blocking_recv().expect("sdp answer from tokio spawn");

    // send back to BEAM so we can include in the POST response body
    (ok(), sdp_answer.to_string())
}

async fn main_loop(mut rx: tokio::sync::mpsc::Receiver<SdpHandshake>) {
    // socket stuff
    let socket = tokio::net::UdpSocket::bind(format!("{}:0", std::env::var("FLY_PUBLIC_IP").expect("ip env")))
        .await
        .expect("binding a random udp port");
    let addr = socket.local_addr().expect("a local socket adddress");

    let (tx2, rx2) = tokio::sync::mpsc::channel(1024);

    spawn(run(socket, rx2));

    loop {
        match rx.recv().await {
            Some(SdpHandshake(_client_type, sdp_offer, tx)) => {
                let rtc = process_sdp(sdp_offer, addr, tx).await.unwrap();

                tx2.send(rtc).await.unwrap();
            }

            None => break,
        }
    }
}

async fn process_sdp(
    sdp_offer: str0m::change::SdpOffer,
    addr: std::net::SocketAddr,
    tx: tokio::sync::oneshot::Sender<str0m::change::SdpAnswer>,
) -> Result<str0m::Rtc, std::io::Error> {
    // create the connection
    let mut rtc = str0m::Rtc::builder().set_ice_lite(true).build();

    // local candidate
    // no trickle ice on SFU side for WHIP/WHEP
    let candidate = str0m::Candidate::host(addr, "udp").expect("a host candidate");
    rtc.add_local_candidate(candidate);

    let sdp_answer = rtc
        .sdp_api()
        .accept_offer(sdp_offer)
        .expect("offer to be accepted");

    // send back to callee so it can give it back
    // to POST request
    tx.send(sdp_answer).expect("sending offer back to parent");

    Ok(rtc)
}

fn load(env: rustler::Env, _: rustler::Term) -> bool {
    env.register::<Link>().is_ok()
}

rustler::init!("Elixir.Floe.SFU", load = load);

async fn run(
    socket: tokio::net::UdpSocket,
    mut rx: tokio::sync::mpsc::Receiver<str0m::Rtc>,
) -> Result<(), str0m::RtcError> {
    let mut clients: Vec<Client> = vec![];
    let mut to_propagate: std::collections::VecDeque<Propagated> =
        std::collections::VecDeque::new();
    let mut buf = vec![0; 2000];

    loop {
        clients.retain(|c| c.rtc.is_alive());

        if let Some(mut client) = spawn_new_client(&mut rx).await {
            // println!("new client {:?}", client);
            for track in clients.iter().flat_map(|c| c.tracks_in.iter()) {
                let weak = std::sync::Arc::downgrade(&track.id);
                client.handle_track_open(weak).await;
            }

            clients.push(client);
        }

        let mut timeout = std::time::Instant::now() + std::time::Duration::from_millis(100);
        for client in clients.iter_mut() {
            let t = poll_until_timeout(client, &mut to_propagate, &socket).await;
            timeout = timeout.min(t);
        }

        if let Some(p) = to_propagate.pop_front() {
            propagate(&p, &mut clients).await;
            continue;
        }

        if let Some(input) = read_socket_input(&socket, &mut buf).await {
            if let Some(client) = clients.iter_mut().find(|c| c.accepts(&input)) {
                client.handle_input(input);
            } else {
                // println!("no client accepts input {:?}", input);
            }
        }

        let now = std::time::Instant::now();
        for client in &mut clients {
            client.handle_input(str0m::Input::Timeout(now));
        }
    }
}

async fn spawn_new_client(rx: &mut tokio::sync::mpsc::Receiver<str0m::Rtc>) -> Option<Client> {
    match rx.try_recv() {
        Ok(rtc) => Some(Client::new(rtc)),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
        _ => panic!("receiver disconnected"),
    }
}

async fn poll_until_timeout(
    client: &mut Client,
    queue: &mut std::collections::VecDeque<Propagated>,
    socket: &tokio::net::UdpSocket,
) -> std::time::Instant {
    loop {
        if !client.rtc.is_alive() {
            return std::time::Instant::now();
        }

        let propagated = client.poll_output(socket).await;

        if let Propagated::Timeout(t) = propagated {
            return t;
        }

        queue.push_back(propagated)
    }
}

async fn propagate(propagated: &Propagated, clients: &mut [Client]) {
    let Some(client_id) = propagated.client_id() else {
        return;
    };

    for client in &mut *clients {
        if client.id == client_id {
            continue;
        }

        match &propagated {
            Propagated::TrackOpen(_, track_in) => {
                client.handle_track_open(track_in.clone()).await
            }
            Propagated::MediaData(_, data) => client.handle_media_data_out(data).await,
            Propagated::KeyframeRequest(_, req, origin, mid_in) => {
                if *origin == client.id {
                    client.handle_keyframe_request(*req, *mid_in).await
                }
            }
            Propagated::Noop | Propagated::Timeout(_) => {}
        }
    }
}

async fn read_socket_input<'a>(
    socket: &tokio::net::UdpSocket,
    buf: &'a mut Vec<u8>,
) -> Option<str0m::Input<'a>> {
    buf.resize(2000, 0);

    match socket.recv_from(buf).await {
        Ok((n, source)) => {
            buf.truncate(n);

            let Ok(contents) = buf.as_slice().try_into() else {
                return None;
            };

            return Some(str0m::Input::Receive(
                std::time::Instant::now(),
                str0m::net::Receive {
                    proto: str0m::net::Protocol::Udp,
                    source,
                    destination: socket.local_addr().unwrap(),
                    contents,
                },
            ));
        }

        Err(e) => match e.kind() {
            _ => panic!("socket read failed {e}"),
        },
    }
}

#[derive(Debug)]
struct Client {
    id: ClientId,
    rtc: str0m::Rtc,
    cid: Option<str0m::channel::ChannelId>,
    tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    chosen_rid: Option<str0m::media::Rid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientId(u64);

impl Deref for ClientId {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
struct TrackIn {
    origin: ClientId,
    mid: str0m::media::Mid,
    kind: str0m::media::MediaKind,
}

#[derive(Debug)]
struct TrackInEntry {
    id: std::sync::Arc<TrackIn>,
    last_keyframe_request: Option<std::time::Instant>,
}

#[derive(Debug)]
struct TrackOut {
    track_in: std::sync::Weak<TrackIn>,
    state: TrackOutState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    fn new(rtc: str0m::Rtc) -> Client {
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

    fn accepts(&self, input: &str0m::Input) -> bool {
        self.rtc.accepts(input)
    }

    fn handle_input(&mut self, input: str0m::Input) {
        if !self.rtc.is_alive() {
            return;
        }

        if let Err(e) = self.rtc.handle_input(input) {
            self.rtc.disconnect();
            panic!("Client ({}) disconnected: {:?}", *self.id, e);
        }
    }

    async fn poll_output(&mut self, socket: &tokio::net::UdpSocket) -> Propagated {
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
                        // Ice disconnect could result in trying to establish a new connection,
                        // but this impl just disconnects directly.
                        self.rtc.disconnect();
                    }
                    Propagated::Noop
                }
                str0m::Event::MediaAdded(e) => {
                    // println!("{:?}", e);

                    self.handle_media_added(e.mid, e.kind).await
                }
                str0m::Event::MediaData(data) => self.handle_media_data_in(data).await,
                str0m::Event::KeyframeRequest(req) => self.handle_incoming_keyframe_req(req).await,
                str0m::Event::ChannelOpen(cid, _) => {
                    self.cid = Some(cid);
                    Propagated::Noop
                }
                // str0m::Event::ChannelData(data) => self.handle_channel_data(data).await,

                // NB: To see statistics, uncomment set_stats_interval() above.
                str0m::Event::MediaIngressStats(data) => {
                    // println!("{:?}", data);
                    Propagated::Noop
                }
                str0m::Event::MediaEgressStats(data) => {
                    // println!("{:?}", data);
                    Propagated::Noop
                }
                str0m::Event::PeerStats(data) => {
                    // println!("{:?}", data);
                    Propagated::Noop
                }
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

        // The Client instance owns the strong reference to the incoming
        // track, all other clients have a weak reference.
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
        // Need to figure out the track_in mid that needs to handle the keyframe request.
        let Some(track_out) = self.tracks_out.iter().find(|t| t.mid() == Some(req.mid)) else {
            return Propagated::Noop;
        };
        let Some(track_in) = track_out.track_in.upgrade() else {
            return Propagated::Noop;
        };

        // This is the rid picked from incoming mediadata, and to which we need to
        // send the keyframe request.
        req.rid = self.chosen_rid;

        Propagated::KeyframeRequest(self.id, req, track_in.origin, track_in.mid)
    }

    async fn handle_track_open(&mut self, track_in: std::sync::Weak<TrackIn>) {
        let track_out = TrackOut {
            track_in,
            state: TrackOutState::ToOpen,
        };
        self.tracks_out.push(track_out);
    }

    async fn handle_media_data_out(&mut self, data: &str0m::media::MediaData) {
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                o.track_in
                    .upgrade()
                    .filter(|i| {
                        if i.kind == str0m::media::MediaKind::Video
                            && data.time.frequency().get() == 48_000
                        {
                            true
                        } else if i.kind == str0m::media::MediaKind::Audio
                            && data.time.frequency().get() == 90_000
                        {
                            true
                        } else {
                            false
                        }
                    })
                    .is_some()
            })
            .and_then(|o| {
                Some(o.track_in.upgrade().unwrap().mid)
            })
        else {
            return;
        };

        if data.rid.is_some() && data.rid != Some("h".into()) {
            // This is where we plug in a selection strategy for simulcast. For
            // now either let rid=None through (which would be no simulcast layers)
            // or "h" if we have simulcast (see commented out code in chat.html).
            return;
        }

        // Remember this value for keyframe requests.
        if self.chosen_rid != data.rid {
            self.chosen_rid = data.rid;
        }

        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };

        // Match outgoing pt to incoming codec.
        let Some(pt) = writer.match_params(data.params) else {
            return;
        };

        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            // println!("client {} failed {:?}", *self.id, e);
            self.rtc.disconnect();
        } 
    }

    async fn handle_keyframe_request(
        &mut self,
        req: str0m::media::KeyframeRequest,
        mid_in: str0m::media::Mid,
    ) {
        let has_incoming_track = self.tracks_in.iter().any(|i| i.id.mid == mid_in);

        // This will be the case for all other client but the one where the track originates.
        if !has_incoming_track {
            return;
        }

        let Some(mut writer) = self.rtc.writer(mid_in) else {
            return;
        };

        if let Err(e) = writer.request_keyframe(req.rid, req.kind) {
            // This can fail if the rid doesn't match any media.
            // println!("request_keyframe failed: {:?}", e);
        }
    }
}

/// Events propagated between client.
#[allow(clippy::large_enum_variant)]

enum Propagated {
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
    fn client_id(&self) -> Option<ClientId> {
        match self {
            Propagated::TrackOpen(c, _)
            | Propagated::MediaData(c, _)
            | Propagated::KeyframeRequest(c, _, _, _) => Some(*c),
            _ => None,
        }
    }
}
