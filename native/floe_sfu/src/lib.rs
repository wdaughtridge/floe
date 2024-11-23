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

#[derive(PartialEq)]
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
    // tx2 goes to WHIP client (how it forwards new media data)
    // rx2 goes to WHEP clients (how clients then receive forwarded media data)
    let (tx2, rx2) = tokio::sync::broadcast::channel::<std::sync::Arc<str0m::media::MediaData>>(16);

    loop {
        match rx.recv().await {
            Some(SdpHandshake(client_type, sdp_offer, tx)) => {
                let tx2 = tx2.clone();
                let rx2 = rx2.resubscribe();
                spawn(handle_sdp_offer(client_type, sdp_offer, tx, tx2, rx2));
            }

            None => break,
        }
    }
}

async fn handle_sdp_offer(
    client_type: ClientType,
    sdp_offer: str0m::change::SdpOffer,
    tx: tokio::sync::oneshot::Sender<str0m::change::SdpAnswer>,
    tx2: tokio::sync::broadcast::Sender<std::sync::Arc<str0m::media::MediaData>>,
    mut rx2: tokio::sync::broadcast::Receiver<std::sync::Arc<str0m::media::MediaData>>,
) -> Result<(), std::io::Error> {
    // create the connection
    let mut rtc = str0m::Rtc::new();

    // socket stuff
    let socket = tokio::net::UdpSocket::bind("10.0.0.60:0")
        .await
        .expect("binding a random udp port");
    let addr = socket.local_addr().expect("a local socket adddress");

    println!("addr {addr}");

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

    let mut buf = Vec::new();

    // now we can loop for data
    loop {
        let timeout = match rtc.poll_output().expect("rtc poll output") {
            str0m::Output::Timeout(v) => v,

            str0m::Output::Transmit(v) => {
                if client_type == ClientType::Whep {
                    println!("{:?}", v);
                }

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
                    str0m::Event::MediaData(media_data) => {
                        if client_type == ClientType::Whip {
                            tx2.send(media_data.into()).expect("sending media data");
                        }
                    }

                    _ => {}
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

        if client_type == ClientType::Whep {
            match rx2.try_recv() {
                Ok(media_data) => {
                    let mid: str0m::media::Mid = media_data.mid;
                    let writer = rtc.writer(mid).unwrap();
                    let pt = writer.payload_params().nth(0).unwrap().pt();
                    let wallclock = std::time::Instant::now();
                    let media_time = media_data.time;

                    println!("media data {:?}", media_data);

                    writer
                        .write(pt, wallclock, media_time, media_data.data.clone())
                        .unwrap();
                }

                Err(_) => {}
            }
        }

        buf.resize(2048, 0);

        let input = match socket.recv_from(&mut buf).await {
            Ok((0, _)) => continue,

            Ok((n, source)) => {
                buf.truncate(n);

                let input = str0m::Input::Receive(
                    std::time::Instant::now(),
                    str0m::net::Receive {
                        proto: str0m::net::Protocol::Udp,
                        source,
                        destination: socket
                            .local_addr()
                            .expect("local socket address from datagram"),
                        contents: buf.as_slice().try_into().expect("rtc input contents"),
                    },
                );

                input
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
