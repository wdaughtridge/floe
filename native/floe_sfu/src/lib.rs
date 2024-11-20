use rustler::Resource;
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::net::Protocol;
use str0m::net::Receive;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

static TOKIO: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
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
    TOKIO.spawn(task)
}

rustler::atoms! {
    ok,
}

struct SdpHandshake(SdpOffer, tokio::sync::oneshot::Sender<SdpAnswer>);

struct Link(tokio::sync::mpsc::Sender<SdpHandshake>);

impl Link {
    fn new(tx: tokio::sync::mpsc::Sender<SdpHandshake>) -> rustler::ResourceArc<Link> {
        rustler::ResourceArc::new(Link(tx))
    }
}

impl Resource for Link {}

#[rustler::nif]
fn start_link() -> (rustler::Atom, rustler::ResourceArc<Link>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<SdpHandshake>(1024);

    spawn(main_loop(rx));

    let link = Link::new(tx);

    (ok(), link)
}

#[rustler::nif]
fn put_new_whep_client(
    sdp_offer: String,
    link: rustler::ResourceArc<Link>,
) -> (rustler::Atom, String) {
    let sdp_offer = SdpOffer::from_sdp_string(&sdp_offer).expect("decoding sdp offer");

    let (tx, rx) = tokio::sync::oneshot::channel::<SdpAnswer>();

    spawn(async move {
        link.0
            .send(SdpHandshake(sdp_offer, tx))
            .await
            .expect("sending to main loop");
    });

    let sdp_answer = rx.blocking_recv().expect("sdp answer from tokio spawn");

    (ok(), sdp_answer.to_string())
}

async fn main_loop(mut rx: tokio::sync::mpsc::Receiver<SdpHandshake>) {
    loop {
        match rx.recv().await {
            Some(SdpHandshake(sdp_offer, tx)) => {
                spawn(handle_sdp_offer(sdp_offer, tx));
            }

            None => break,
        }
    }
}

async fn handle_sdp_offer(
    sdp_offer: SdpOffer,
    tx: tokio::sync::oneshot::Sender<SdpAnswer>,
) -> Result<(), std::io::Error> {
    let mut rtc = Rtc::new();

    let socket = tokio::net::UdpSocket::bind(format!("10.0.0.60:0"))
        .await
        .expect("binding a random udp port");

    let addr = socket.local_addr().expect("a local socket adddress");
    println!("{0}", addr);

    let candidate = Candidate::host(addr, "udp").expect("a host candidate");

    rtc.add_local_candidate(candidate);

    let sdp_answer = rtc
        .sdp_api()
        .accept_offer(sdp_offer)
        .expect("offer to be accepted");

    tx.send(sdp_answer).expect("sending offer back to parent");

    let mut buf = Vec::new();

    loop {
        let timeout = match rtc.poll_output().expect("rtc poll output") {
            Output::Timeout(v) => v,

            Output::Transmit(v) => {
                socket
                    .send_to(&v.contents, v.destination)
                    .await
                    .expect("socket send to");

                continue;
            }

            Output::Event(v) => {
                if v == Event::IceConnectionStateChange(IceConnectionState::Disconnected) {
                    return Ok::<(), tokio::io::Error>(());
                }

                continue;
            }
        };

        let timeout = timeout - std::time::Instant::now();

        if timeout.is_zero() {
            rtc.handle_input(Input::Timeout(std::time::Instant::now()))
                .expect("socket handle input");

            continue;
        }

        buf.resize(2048, 0);

        let input = match socket.recv_from(&mut buf).await {
            Ok((0, _)) => continue,

            Ok((n, source)) => {
                buf.truncate(n);

                let input = Input::Receive(
                    std::time::Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
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
