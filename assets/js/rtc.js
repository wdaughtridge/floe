const rtcConfig = { iceServers: [{ urls: "stun:stun.l.google.com:19302" }] };
const rtc = new RTCPeerConnection(rtcConfig);
const candidates = [];
const constraints = {
  audio: {
    echoCancellation: { exact: true },
  },
  video: {
    width: 1280,
    height: 720,
  },
};

let patchEndpoint;
let bearer;

async function sendCandidate(candidate) {
  const response = await fetch(patchEndpoint, {
    method: "PATCH",
    cache: "no-cache",
    headers: {
      "Content-Type": "application/trickle-ice-sdpfrag",
      Authorization: bearer,
    },
    body: candidate,
  });

  if (response.status !== 204) {
    console.error(
      `Failed to send ICE, status: ${response.status}, candidate:`,
      candidate,
    );
  }
}

export async function startRtc(whip) {
  const stream_id = document.getElementById("stream_id").value;
  const token = document.getElementById("token").value;
  bearer = `Bearer ${token}`;

  let fetchUrl;
  if (whip) {
    fetchUrl = `/api/whip?stream_id=${stream_id}`;

    document.getElementById("join").disabled = true;

    mediaStream = await navigator.mediaDevices.getUserMedia(constraints);
    mediaStream
      .getTracks()
      .forEach((track) => rtc.addTrack(track, mediaStream));
    document.getElementById("feed").srcObject = mediaStream;
  } else {
    fetchUrl = `/api/whep?stream_id=${stream_id}`;

    document.getElementById("stream").disabled = true;

    rtc.ontrack = (e) => {
      const track = e.track;
      const el = document.getElementById("feed");

      setTimeout(() => {
        let media;

        if (el.srcObject != null) {
          media = el.srcObject;
        } else {
          media = new MediaStream();
        }

        media.addTrack(track);
        el.srcObject = media;
      }, 1);
    };

    rtc.addTransceiver("video", {
      direction: "recvonly",
    });

    rtc.addTransceiver("audio", {
      direction: "recvonly",
    });
  }

  rtc.onicecandidate = (event) => {
    if (event.candidate == null) {
      return;
    }

    const candidate = JSON.stringify(event.candidate);

    if (patchEndpoint === undefined) {
      candidates.push(candidate);
    } else {
      sendCandidate(candidate);
    }
  };

  const offer = await rtc.createOffer();

  rtc.setLocalDescription(offer);

  const res = await fetch(fetchUrl, {
    credentials: "include",
    method: "POST",
    headers: {
      "Content-Type": "application/sdp",
      Accept: "application/sdp",
      Authorization: bearer,
    },
    body: offer.sdp,
  });

  if (res.status === 201) {
    patchEndpoint = res.headers.get("location");
  } else {
    console.error(`Failed to initialize connection, status: ${res.status}`);
    return;
  }

  for (const candidate of candidates) {
    sendCandidate(candidate);
  }

  const answer = await res.text();

  rtc.setRemoteDescription({
    type: "answer",
    sdp: answer,
  });
}
