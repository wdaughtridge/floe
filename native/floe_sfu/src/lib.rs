mod client;

use client::*;
use log::{error, info};
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

struct Link(
    tokio::sync::mpsc::Sender<SdpHandshake>,
    tokio::sync::mpsc::Sender<String>,
);

// this is a link back to the main loop for a particular
// stream. this is passed back to the BEAM such that any
// process with the reference to this can add new WHIP
// or WHEP clients.
impl Link {
    fn new(
        handshake_sender: tokio::sync::mpsc::Sender<SdpHandshake>,
        candidate_sender: tokio::sync::mpsc::Sender<String>,
    ) -> rustler::ResourceArc<Link> {
        rustler::ResourceArc::new(Link(handshake_sender, candidate_sender))
    }
}

impl Resource for Link {}

// entry point probably started by a WHIP client
#[rustler::nif]
fn start_link() -> (rustler::Atom, rustler::ResourceArc<Link>) {
    let (handshake_sender, handshake_receiver) = tokio::sync::mpsc::channel::<SdpHandshake>(16);
    let (candidate_sender, candidate_receiver) = tokio::sync::mpsc::channel::<String>(16);

    spawn(main_loop(handshake_receiver, candidate_receiver));

    let link = Link::new(handshake_sender, candidate_sender);

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

#[rustler::nif]
fn put_new_remote_candidate(
    trickle_ice: String,
    link: rustler::ResourceArc<Link>,
) -> rustler::Atom {
    handle_trickle_ice(trickle_ice, link)
}

fn handle_trickle_ice(trickle_ice: String, link: rustler::ResourceArc<Link>) -> rustler::Atom {
    match serde_json::from_str::<str0m::Candidate>(&trickle_ice) {
        Ok(_) => {
            spawn(async move {
                match link.1.send(trickle_ice).await {
                    Ok(_) => {}

                    Err(e) => error!("{e}"),
                }
            });
        }

        Err(_) => {
            error!("could not deserialize candidate {trickle_ice}");
        }
    }

    ok()
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
    let (sdp_sender, sdp_receiver) = tokio::sync::oneshot::channel::<str0m::change::SdpAnswer>();

    // start an event loop for this client to receive data
    spawn(async move {
        link.0
            .send(SdpHandshake(client_type, sdp_offer, sdp_sender))
            .await
            .expect("sending to main loop");
    });

    // get the answer from the spawned task
    let sdp_answer = sdp_receiver
        .blocking_recv()
        .expect("sdp answer from tokio spawn");

    let sdp_answer = sdp_answer.to_string();

    // send back to BEAM so we can include in the POST response body
    (ok(), sdp_answer)
}

async fn main_loop(
    mut handshake_receiver: tokio::sync::mpsc::Receiver<SdpHandshake>,
    mut candidate_receiver: tokio::sync::mpsc::Receiver<String>,
) {
    // socket stuff
    let socket = tokio::net::UdpSocket::bind(format!(
        "{}:0",
        std::env::var("FLY_PUBLIC_IP").expect("ip env")
    ))
    .await
    .expect("binding a random udp port");
    let addr = socket.local_addr().expect("a local socket adddress");

    let (loop_handshake_sender, loop_handshake_receiver) = tokio::sync::mpsc::channel(16);
    let (loop_candidate_sender, loop_candidate_receiver) = tokio::sync::mpsc::channel(16);

    spawn(run(
        socket,
        loop_handshake_receiver,
        loop_candidate_receiver,
    ));

    spawn(async move {
        loop {
            match candidate_receiver.recv().await {
                Some(trickle_ice) => {
                    loop_candidate_sender.send(trickle_ice).await.unwrap();
                }

                None => {}
            }
        }
    });

    loop {
        match handshake_receiver.recv().await {
            Some(SdpHandshake(_client_type, sdp_offer, tx)) => {
                let rtc = process_sdp(sdp_offer, addr, tx).await.unwrap();

                loop_handshake_sender.send(rtc).await.unwrap();
            }

            None => {}
        }
    }
}

async fn process_sdp(
    sdp_offer: str0m::change::SdpOffer,
    addr: std::net::SocketAddr,
    tx: tokio::sync::oneshot::Sender<str0m::change::SdpAnswer>,
) -> Result<str0m::Rtc, std::io::Error> {
    // create the connection
    let mut rtc = str0m::Rtc::builder().build();

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
    env_logger::init();

    env.register::<Link>().is_ok()
}

rustler::init!("Elixir.Floe.SFU", load = load);

async fn run(
    socket: tokio::net::UdpSocket,
    mut rx: tokio::sync::mpsc::Receiver<str0m::Rtc>,
    mut rx2: tokio::sync::mpsc::Receiver<String>,
) -> Result<(), str0m::RtcError> {
    let mut clients: Vec<Client> = vec![];
    let mut to_propagate: std::collections::VecDeque<Propagated> =
        std::collections::VecDeque::new();
    let mut buf = vec![0; 2000];

    loop {
        // clients.retain(|c| c.rtc.is_alive());

        if let Some(trickle_ice) = new_remote_candidate(&mut rx2).await {
            for client in clients.iter_mut() {
                let candidate = serde_json::from_str::<str0m::Candidate>(&trickle_ice).unwrap();

                info!("new candidate {:?} for client {:?}", candidate, client);

                client.handle_new_candidate(candidate);
            }
        }

        if let Some(mut client) = spawn_new_client(&mut rx).await {
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
            }
        }

        let now = std::time::Instant::now();
        for client in &mut clients {
            client.handle_input(str0m::Input::Timeout(now));
        }
    }
}

async fn new_remote_candidate(rx: &mut tokio::sync::mpsc::Receiver<String>) -> Option<String> {
    match rx.try_recv() {
        Ok(candidate) => Some(candidate),

        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,

        _ => panic!("receiver disconnected"),
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
            Propagated::TrackOpen(_, track_in) => client.handle_track_open(track_in.clone()).await,
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
