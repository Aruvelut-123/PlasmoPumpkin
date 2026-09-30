//! A whole voice session, from a control-plane message to a relayed audio frame.
//!
//! These tests drive the *public* API in the order a running server does it: parse a
//! `plasmo:voice/v2` control message, mint the player's secret, hand the client the
//! encoded `ConnectionPacket`, then let the client's UDP datagrams create and use the
//! connection. They exist because the WASI glue that wires those pieces together only
//! compiles for the component target, so the contract between them has to be pinned
//! somewhere the host can run.

use plasmo_voice_core::wire::{PingPacket, PlayerAudioPacket};
use plasmo_voice_core::{PacketDirection, TcpCodec, TcpPacket, UdpCodec, UdpPacket};
use plasmo_voice_plugin::server::VoiceServer;
use plasmo_voice_plugin::{VoiceIpc, player_connect_reply};
use uuid::Uuid;

const SERVER_SECRET: Uuid = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
const ALICE: Uuid = Uuid::from_u128(0x0000_000a_11ce);
const BOB: Uuid = Uuid::from_u128(0x0000_0000_0b0b);

const SERVER_IP: &str = "203.0.113.7";
const SERVER_PORT: u16 = 8830;

/// A fixed server-wide AES key, so tests stay deterministic and the data plane
/// does not touch the guest's random source.
fn test_aes_key() -> plasmo_voice_plugin::crypto::AesKey {
    plasmo_voice_plugin::crypto::AesKey::from_hex("00112233445566778899aabbccddeeff")
        .expect("the test key is 32 lowercase hex chars")
}

/// The server under test, on a fixed secret.
fn server() -> VoiceServer {
    VoiceServer::new(SERVER_SECRET, test_aes_key())
}

/// Sends one `player-connect` control message and returns the secret the client
/// would learn from the encoded `ConnectionPacket`.
///
/// This is the control-plane half of the session: exactly what the WASI IPC handler
/// does, minus the `with_runtime` locking.
fn connect(server: &mut VoiceServer, expected: Uuid, display_name: &str) -> Uuid {
    let message = format!(
        "plasmo:voice/v2\nplayer-connect\nplayer={expected}\nname={display_name}\nip={SERVER_IP}"
    );
    let Some(VoiceIpc::PlayerConnect { player, name, ip }) = VoiceIpc::parse(message.as_bytes())
    else {
        panic!("the message must parse as a player-connect");
    };

    let player_id = Uuid::parse_str(&player).expect("the control message carries a UUID");
    assert_eq!(player_id, expected);
    let secret = server.register_player(player_id, name);
    let reply = player_connect_reply(player_id, secret, SERVER_PORT, ip.as_deref()).expect("reply");

    // The caller forwards the reply's bytes to the client, which decodes a
    // clientbound `ConnectionPacket` out of them.
    let hex = reply
        .lines()
        .find_map(|line| line.strip_prefix("packet="))
        .expect("the reply carries a packet line");
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex digits"))
        .collect();
    match TcpCodec::new()
        .decode(&bytes, PacketDirection::Client)
        .expect("decode ok")
        .expect("a packet")
    {
        TcpPacket::Connection(packet) => {
            assert_eq!(packet.secret, secret, "the client learns its own secret");
            assert_eq!(packet.ip, SERVER_IP);
            assert_eq!(packet.port, i32::from(SERVER_PORT));
        }
        other => panic!("expected a connection packet, got {other:?}"),
    }

    secret
}

/// A client's registration ping: an empty payload plus the endpoint it wants the
/// server to be reachable at, which is what upstream's client sends until it hears
/// back.
fn registration_ping(codec: &UdpCodec, secret: Uuid) -> Vec<u8> {
    codec
        .encode(
            &UdpPacket::Ping(PingPacket {
                time: 1,
                server_ip: Some(SERVER_IP.to_string()),
                server_port: Some(SERVER_PORT),
            }),
            secret,
            1,
        )
        .expect("encode")
}

#[test]
fn a_whole_session_connects_relays_and_disconnects() {
    let codec = UdpCodec::new();
    let mut server = server();

    // 1. Control plane: two players join and each learns its own secret.
    let alice = connect(&mut server, ALICE, "Alice");
    let bob = connect(&mut server, BOB, "Bob");
    assert_ne!(alice, bob, "one secret per player");
    assert_eq!(server.registered_player_count(), 2);
    assert_eq!(
        server.connection_count(),
        0,
        "registering a player is not a UDP connection yet"
    );

    // 2. Data plane: their first datagrams create the connections. A ping is never
    //    answered — echoing it would make a real client ping-pong forever — but the
    //    connection's birth is what triggers the control-plane registration burst,
    //    which the caller delivers over the `plasmo:voice` channel.
    let alice_ping = registration_ping(&codec, alice);
    let handled = server.handle_datagram("198.51.100.10:40000", &alice_ping);
    assert!(handled.outgoing.is_empty());
    assert!(handled.was_ping);
    assert_eq!(server.connection_count(), 1);
    let ids: Vec<u8> = handled
        .control
        .iter()
        .map(|message| message.payload[0])
        .collect();
    assert_eq!(
        ids,
        vec![3, 7, 8],
        "config, then player list, then the player-info broadcast"
    );
    assert_eq!(
        server
            .client_by_secret(&alice)
            .and_then(|client| client.connection_address.as_deref()),
        Some("203.0.113.7:8830"),
        "the ping's endpoint is recorded, but never used as a send target"
    );
    assert_eq!(
        server.client_by_secret(&alice).map(|c| c.address.as_str()),
        Some("198.51.100.10:40000"),
        "datagrams go to the address they came from"
    );

    let bob_ping = registration_ping(&codec, bob);
    server.handle_datagram("198.51.100.20:40001", &bob_ping);
    assert_eq!(server.connection_count(), 2);
    assert_eq!(server.dropped(), 0);

    // 3. A stranger with a valid-looking secret of its own is dropped: only the
    //    control plane can make a secret real.
    let stranger = Uuid::from_u128(0xdead_beef);
    let stranger_ping = registration_ping(&codec, stranger);
    let handled = server.handle_datagram("198.51.100.30:40002", &stranger_ping);
    assert!(handled.outgoing.is_empty());
    assert_eq!(server.connection_count(), 2);
    assert_eq!(server.dropped(), 1);

    // 4. Audio from a connected player reaches every *other* connection in range, as a
    //    clientbound `SourceAudioPacket` re-keyed to that connection's secret so only its
    //    owner can read it — and the speaker itself gets a `SelfAudioInfoPacket` instead.
    //    Upstream's relay is proximity-filtered, so both players have to be placed first.
    server.set_position(&ALICE, Some("world".to_string()), (0.0, 64.0, 0.0));
    server.set_position(&BOB, Some("world".to_string()), (8.0, 64.0, 0.0));
    let activation = server.config().activations()[0].id;
    let audio = PlayerAudioPacket {
        sequence_number: 7,
        data: vec![0xde, 0xad, 0xbe, 0xef],
        activation_id: activation,
        distance: 16,
        stereo: false,
    };
    let frame = codec
        .encode(&UdpPacket::PlayerAudio(audio.clone()), alice, 0)
        .expect("encode");
    let handled = server.handle_datagram("198.51.100.10:40000", &frame);
    assert_eq!(
        handled.outgoing.len(),
        2,
        "bob hears alice, and alice is told about her own stream"
    );
    let to_bob = handled
        .outgoing
        .iter()
        .find(|datagram| datagram.to == "198.51.100.20:40001")
        .expect("bob is listening");
    let envelope = codec
        .decode_header(&to_bob.data)
        .expect("header")
        .expect("envelope");
    assert_eq!(envelope.secret, bob, "bob's own key");
    match codec
        .decode(&to_bob.data, PacketDirection::Client)
        .expect("decode ok")
        .expect("packet")
    {
        UdpPacket::SourceAudio(source) => {
            assert_eq!(source.sequence_number, 7);
            assert_eq!(source.data, vec![0xde, 0xad, 0xbe, 0xef]);
            assert_eq!(source.source_id, server.source_id_of(&ALICE).unwrap());
            assert_eq!(source.distance, 16);
        }
        other => panic!("expected source audio, got {other:?}"),
    }
    let to_alice = handled
        .outgoing
        .iter()
        .find(|datagram| datagram.to == "198.51.100.10:40000")
        .expect("alice is told about her own stream");
    match codec
        .decode(&to_alice.data, PacketDirection::Client)
        .expect("decode ok")
        .expect("packet")
    {
        UdpPacket::SelfAudioInfo(info) => {
            assert_eq!(info.sequence_number, 7);
            assert!(info.data.is_none(), "the payload is never echoed back");
        }
        other => panic!("expected self audio info, got {other:?}"),
    }

    // 5. Keep-alive keeps both connections warm: one empty ping each, addressed to
    //    the connection's own secret.
    let sweep = server.keep_alive(plasmo_voice_plugin::server::now_ms(), 15_000);
    assert_eq!(sweep.outgoing.len(), 2);
    assert!(sweep.control.is_empty(), "nobody timed out");
    for ping in &sweep.outgoing {
        assert!(
            matches!(
                codec
                    .decode(&ping.data, PacketDirection::Server)
                    .expect("decode ok")
                    .expect("packet"),
                UdpPacket::Ping(_)
            ),
            "a keep-alive is an empty ping"
        );
    }

    // 6. Alice leaves: her secret stops working and her connection is gone, while
    //    bob keeps his.
    assert_eq!(server.unregister_player(&ALICE), Some(alice));
    assert_eq!(server.registered_player_count(), 1);
    assert_eq!(server.connection_count(), 1);
    let handled = server.handle_datagram("198.51.100.10:40000", &alice_ping);
    assert!(handled.outgoing.is_empty());
    assert_eq!(server.connection_count(), 1, "alice cannot come back in");
    assert_eq!(server.dropped(), 2);

    // Bob's audio is accepted rather than dropped, but it reaches nobody: he is the only
    // player left, so the only datagram is his own self-info.
    let frame = codec
        .encode(&UdpPacket::PlayerAudio(audio), bob, 0)
        .expect("encode");
    let handled = server.handle_datagram("198.51.100.20:40001", &frame);
    assert_eq!(handled.outgoing.len(), 1);
    assert_eq!(handled.outgoing[0].to, "198.51.100.20:40001");
    assert!(
        matches!(
            codec
                .decode(&handled.outgoing[0].data, PacketDirection::Client)
                .expect("decode ok")
                .expect("packet"),
            UdpPacket::SelfAudioInfo(_)
        ),
        "the speaker's own stream info, not a relay"
    );
    assert_eq!(server.dropped(), 2, "bob is still a valid client");
}

#[test]
fn a_client_that_changes_address_keeps_its_connection() {
    let codec = UdpCodec::new();
    let mut server = server();
    let alice = connect(&mut server, ALICE, "Alice");
    let ping = registration_ping(&codec, alice);

    server.handle_datagram("198.51.100.10:40000", &ping);
    server.handle_datagram("198.51.100.99:50000", &ping);

    assert_eq!(
        server.connection_count(),
        1,
        "a reconnecting client must not occupy two slots"
    );
    let client = server.client_by_secret(&alice).expect("connection");
    assert_eq!(client.address, "198.51.100.99:50000");
    assert!(
        server.client_by_address("198.51.100.10:40000").is_none(),
        "the stale address must not resolve to a live connection"
    );
}

#[test]
fn a_disconnected_player_is_forgotten_but_can_reconnect() {
    let codec = UdpCodec::new();
    let mut server = server();
    let first = connect(&mut server, ALICE, "Alice");
    server.handle_datagram("198.51.100.10:40000", &registration_ping(&codec, first));

    server
        .unregister_player(&ALICE)
        .expect("alice was connected");
    assert_eq!(server.connection_count(), 0);

    // A fresh control-plane message mints a new secret; the old one stays dead.
    let second = connect(&mut server, ALICE, "Alice");
    assert_ne!(second, first);
    assert!(server.player_for_secret(&first).is_none());
    assert_eq!(server.player_for_secret(&second), Some(ALICE));
}
