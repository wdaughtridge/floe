use rustler::Resource;

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
    let (tx2, rx2) = tokio::sync::broadcast::channel(1024);
    let (tx3, rx3) = tokio::sync::broadcast::channel(16);

    loop {
        match rx.recv().await {
            Some(SdpHandshake(client_type, sdp_offer, tx)) => {
                let tx2 = tx2.clone();
                let rx2 = rx2.resubscribe();

                let tx3 = tx3.clone();
                let rx3 = rx3.resubscribe();

                let (rtc, socket) = process_sdp(sdp_offer, tx).await.unwrap();

                spawn(handler(rtc, socket, client_type, tx2, rx2, tx3, rx3));
            }

            None => break,
        }
    }
}

async fn process_sdp(
    sdp_offer: str0m::change::SdpOffer,
    tx: tokio::sync::oneshot::Sender<str0m::change::SdpAnswer>,
) -> Result<(str0m::Rtc, tokio::net::UdpSocket), std::io::Error> {
    // create the connection
    let mut rtc = str0m::Rtc::new();
    // socket stuff
    let socket = tokio::net::UdpSocket::bind("10.0.0.60:0")
        .await
        .expect("binding a random udp port");
    let addr = socket.local_addr().expect("a local socket adddress");

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

    Ok((rtc, socket))
}

async fn handler(
    mut rtc: str0m::Rtc,
    socket: tokio::net::UdpSocket,
    client_type: ClientType,
    tx: tokio::sync::broadcast::Sender<std::sync::Arc<str0m::media::MediaData>>,
    mut rx: tokio::sync::broadcast::Receiver<std::sync::Arc<str0m::media::MediaData>>,
    tx2: tokio::sync::broadcast::Sender<bool>,
    mut rx2: tokio::sync::broadcast::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let mut buf = Vec::new();

    let mut video_mid: Option<str0m::media::Mid> = None;
    let mut audio_mid: Option<str0m::media::Mid> = None;

    // now we can loop for data
    loop {
        if client_type == ClientType::Whip {
            match rx2.try_recv() {
                Ok(true) => {
                    if let Some(mid) = video_mid {
                        let mut writer = rtc.writer(mid).unwrap();

                        writer
                            .request_keyframe(None, str0m::media::KeyframeRequestKind::Pli)
                            .unwrap();
                    }
                }

                Ok(false) => {}

                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    panic!("lagged by {n}");
                }

                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}

                Err(e) => {
                    eprintln!("{:?} rx2 try_recv {}", client_type, e);
                }
            }
        } else if client_type == ClientType::Whep {
            match rx.try_recv() {
                Ok(media_data) => {
                    let mid = if media_data.time.frequency()
                        == str0m::media::Frequency::new(90000).unwrap()
                    {
                        video_mid
                    } else {
                        audio_mid
                    };

                    if let Some(mid) = mid {
                        let writer = rtc.writer(mid).unwrap();
                        let pt = writer.match_params(media_data.params).unwrap();
                        let media_time = media_data.time;
                        let wallclock = media_data.network_time;

                        writer
                            .write(pt, wallclock, media_time, media_data.data.clone())
                            .unwrap();
                    }
                }

                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    panic!("lagged by {n}");
                }

                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}

                Err(e) => {
                    eprintln!("{:?} rx2 try_recv {}", client_type, e);
                }
            }
        }

        let timeout = match rtc.poll_output().expect("rtc poll output") {
            str0m::Output::Timeout(v) => v,

            str0m::Output::Transmit(v) => {
                socket
                    .send_to(&v.contents, v.destination)
                    .await
                    .expect("socket send to");

                continue;
            }

            str0m::Output::Event(v) => {
                if v == str0m::Event::IceConnectionStateChange(
                    str0m::IceConnectionState::Disconnected,
                ) {
                    return Ok::<(), tokio::io::Error>(());
                }

                match v {
                    str0m::Event::PeerStats(e) => {
                        println!("{:?} {:?}", client_type, e);
                    }

                    str0m::Event::MediaData(media_data) => {
                        if client_type == ClientType::Whip {
                            tx.send(media_data.into()).expect("sending media data");
                        }
                    }

                    str0m::Event::MediaAdded(m) => {
                        println!("{:?} {:?}", client_type, m);

                        match m.kind {
                            str0m::media::MediaKind::Audio => {
                                audio_mid = Some(m.mid);
                            }

                            str0m::media::MediaKind::Video => {
                                video_mid = Some(m.mid);
                            }
                        }
                    }

                    str0m::Event::KeyframeRequest(_k) => {
                        tx2.send(true).expect("sending keyframe request");
                    }

                    e => {
                        println!("{:?} {:?}", client_type, e);
                    }
                }

                continue;
            }
        };

        let timeout = timeout - std::time::Instant::now();

        if timeout.is_zero() {
            rtc.handle_input(str0m::Input::Timeout(std::time::Instant::now()))
                .expect("socket handle input");

            continue;
        }

        buf.resize(2048, 0);

        let input = match socket.recv_from(&mut buf).await {
            Ok((0, _)) => continue,

            Ok((n, source)) => {
                buf.truncate(n);

                str0m::Input::Receive(
                    std::time::Instant::now(),
                    str0m::net::Receive {
                        proto: str0m::net::Protocol::Udp,
                        source,
                        destination: socket
                            .local_addr()
                            .expect("local socket address from datagram"),
                        contents: buf.as_slice().try_into().expect("rtc input contents"),
                    },
                )
            }

            Err(_) => continue,
        };

        rtc.handle_input(input).expect("rtc handle input");
    }
}

fn load(env: rustler::Env, _: rustler::Term) -> bool {
    env.register::<Link>().is_ok()
}

rustler::init!("Elixir.Floe.SFU", load = load);
