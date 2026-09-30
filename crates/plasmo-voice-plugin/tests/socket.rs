//! The data plane over **real sockets**.
//!
//! `lifecycle.rs` drives `VoiceServer` directly, which pins the protocol logic but
//! never touches a socket. This test goes through [`VoiceRuntime::pump`] and three real
//! `UdpSocket`s (the server plus two clients), so the receive loop, the address parsing
//! and the send path are covered too — that is the half where a `parse::<SocketAddr>`
//! mistake would silently drop every outgoing frame while the unit tests stayed green.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use plasmo_voice_core::wire::{PingPacket, PlayerAudioPacket};
use plasmo_voice_core::{PacketDirection, UdpCodec, UdpPacket};
use plasmo_voice_plugin::runtime::{VoiceRuntime, bind};
use plasmo_voice_plugin::server::VoiceServer;
use uuid::Uuid;

const SERVER_SECRET: Uuid = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
const ALICE: Uuid = Uuid::from_u128(0x0000_000a_11ce);
const BOB: Uuid = Uuid::from_u128(0x0000_0000_0b0b);

/// A non-blocking client socket, so a test never blocks on a datagram that is not
/// coming (which is how a broken relay should fail: with a timeout, not a hang).
fn client() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("client bind");
    socket
        .set_nonblocking(true)
        .expect("client socket non-blocking");
    socket
}

/// Reads everything already queued on `socket`, discarding it.
fn drain(socket: &UdpSocket) {
    let mut buffer = [0u8; 2048];
    while socket.recv_from(&mut buffer).is_ok() {}
}

/// Pumps the runtime for up to `timeout` until `ready` holds.
fn pump_until(
    runtime: &mut VoiceRuntime,
    timeout: Duration,
    mut ready: impl FnMut(&VoiceRuntime) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        runtime.pump();
        if ready(runtime) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    ready(runtime)
}

#[test]
fn audio_from_one_client_reaches_the_other_over_real_sockets() {
    let codec = UdpCodec::new();
    let (socket, port) = bind(0).expect("voice socket");
    let mut runtime = VoiceRuntime::new();
    runtime.start(socket, VoiceServer::new(SERVER_SECRET), "/tmp".to_string());
    // Keep the sweep from retiring anything mid-test; the timeout behaviour is covered
    // by the `server.rs` unit tests with an injected clock.
    runtime.set_keep_alive_timeout(60_000);

    let alice_secret = runtime
        .protocol_mut()
        .expect("protocol")
        .register_player(ALICE, Some("Alice".into()));
    let bob_secret = runtime
        .protocol_mut()
        .expect("protocol")
        .register_player(BOB, Some("Bob".into()));
    assert_ne!(alice_secret, bob_secret);

    let alice = client();
    let bob = client();
    let server = format!("127.0.0.1:{port}");

    // Both clients announce themselves the way upstream's client does: a ping whose
    // secret the server has already been told about.
    let ping = |secret: Uuid| {
        codec
            .encode(&UdpPacket::Ping(PingPacket::new(None, 0)), secret, 1)
            .expect("encode")
    };
    alice
        .send_to(&ping(alice_secret), &server)
        .expect("alice ping");
    bob.send_to(&ping(bob_secret), &server).expect("bob ping");

    assert!(
        pump_until(&mut runtime, Duration::from_secs(5), |runtime| {
            runtime.connection_count() == 2
        }),
        "both registered players must end up connected"
    );
    assert_eq!(runtime.received(), 2);

    // The server pings both clients on its own schedule; that first ping is what makes a
    // real client consider itself connected. Clear it so the next frame is the audio.
    assert!(
        pump_until(&mut runtime, Duration::from_secs(1), |runtime| runtime
            .sent()
            >= 2),
        "the keep-alive sweep must reach both clients"
    );
    drain(&alice);
    drain(&bob);

    // Alice speaks.
    let audio = PlayerAudioPacket {
        sequence_number: 7,
        data: vec![0xde, 0xad, 0xbe, 0xef],
        activation_id: Uuid::from_u128(0x5),
        distance: 16,
        stereo: false,
    };
    alice
        .send_to(
            &codec
                .encode(&UdpPacket::PlayerAudio(audio.clone()), alice_secret, 0)
                .expect("encode"),
            &server,
        )
        .expect("alice speaks");

    let mut buffer = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut relayed = None;
    while Instant::now() < deadline && relayed.is_none() {
        runtime.pump();
        match bob.recv_from(&mut buffer) {
            Ok((len, _)) => {
                let frame = buffer[..len].to_vec();
                // A keep-alive ping may interleave with the audio; skip it.
                if matches!(
                    codec
                        .decode(&frame, PacketDirection::Server)
                        .expect("decode ok")
                        .expect("packet"),
                    UdpPacket::Ping(_)
                ) {
                    continue;
                }
                relayed = Some(frame);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    let frame = relayed.expect("bob must hear alice");
    let envelope = codec
        .decode_header(&frame)
        .expect("header")
        .expect("envelope");
    assert_eq!(
        envelope.secret, bob_secret,
        "the relayed frame is addressed to bob's own secret"
    );
    assert_eq!(
        codec
            .decode(&frame, PacketDirection::Server)
            .expect("decode ok")
            .expect("packet"),
        UdpPacket::PlayerAudio(audio)
    );

    // Alice must not hear her own voice back.
    let mut own = [0u8; 2048];
    while let Ok((len, _)) = alice.recv_from(&mut own) {
        let frame = own[..len].to_vec();
        assert!(
            matches!(
                codec
                    .decode(&frame, PacketDirection::Server)
                    .expect("decode ok")
                    .expect("packet"),
                UdpPacket::Ping(_)
            ),
            "alice received something other than a keep-alive"
        );
    }
}

#[test]
fn a_stranger_never_gets_a_connection_or_a_reply() {
    let codec = UdpCodec::new();
    let (socket, port) = bind(0).expect("voice socket");
    let mut runtime = VoiceRuntime::new();
    runtime.start(socket, VoiceServer::new(SERVER_SECRET), "/tmp".to_string());

    let stranger = client();
    let server = format!("127.0.0.1:{port}");
    let frame = codec
        .encode(
            &UdpPacket::Ping(PingPacket::new(None, 0)),
            Uuid::from_u128(0xdead_beef),
            1,
        )
        .expect("encode");
    stranger.send_to(&frame, &server).expect("stranger ping");

    assert!(
        pump_until(&mut runtime, Duration::from_secs(5), |runtime| {
            runtime.received() == 1
        }),
        "the datagram must be counted"
    );
    // Give a would-be reply time to arrive, then prove nothing did.
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        runtime.pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(runtime.connection_count(), 0);
    assert_eq!(runtime.sent(), 0, "an unknown secret is never answered");
    let mut buffer = [0u8; 2048];
    assert!(
        stranger.recv_from(&mut buffer).is_err(),
        "the stranger must receive nothing at all"
    );
}
