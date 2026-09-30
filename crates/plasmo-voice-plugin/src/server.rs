//! The voice server's protocol logic, decoupled from the socket.
//!
//! [`VoiceServer`] owns the UDP *semantics* (packet decoding, per-connection
//! secrets, audio fan-out) and never touches a socket itself. `lib.rs` drives it
//! from the plugin's `std::net::UdpSocket`, while tests drive it directly with
//! synthetic datagrams — so the interesting logic is covered on the host target
//! even though the real server runs on `wasm32-wasip2`.

use std::collections::HashMap;
use std::net::SocketAddr;

use plasmo_voice_core::{PacketDirection, UdpCodec, UdpPacket, VoiceError};
use uuid::Uuid;

/// A client known to the voice server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// The secret UUID carried in every UDP frame header from this client.
    pub secret: Uuid,
    /// `host:port` the client is speaking from.
    pub address: String,
    /// The player this connection belongs to, when bound over TCP.
    pub player_id: Option<String>,
}

/// One outgoing datagram produced by [`VoiceServer::handle_datagram`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub to: String,
    pub data: Vec<u8>,
}

/// Decides what the server does with an inbound datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handled {
    /// Datagrams to send back (ping replies, forwarded audio).
    pub outgoing: Vec<Outgoing>,
    /// Set when the packet was a ping, carrying its payload for tests/logs.
    pub was_ping: bool,
}

impl Handled {
    fn nothing() -> Self {
        Self {
            outgoing: Vec::new(),
            was_ping: false,
        }
    }
}

/// The Plasmo Voice UDP server.
///
/// Connections are keyed by secret (`connectionBySecret` upstream); a client
/// also maps to at most one connection keyed by address.
#[derive(Debug)]
pub struct VoiceServer {
    codec: UdpCodec,
    /// secret -> client
    by_secret: HashMap<Uuid, Client>,
    /// address -> secret
    by_address: HashMap<String, Uuid>,
    /// The server-wide secret advertised in `ConnectionPacket`.
    server_secret: Uuid,
    /// Datagrams dropped before decoding (wrong magic / unknown id).
    dropped: u64,
}

impl VoiceServer {
    /// Creates a server whose advertised secret is `server_secret`.
    #[must_use]
    pub fn new(server_secret: Uuid) -> Self {
        Self {
            codec: UdpCodec::new(),
            by_secret: HashMap::new(),
            by_address: HashMap::new(),
            server_secret,
            dropped: 0,
        }
    }

    /// The secret this server advertises to clients.
    #[must_use]
    pub const fn server_secret(&self) -> Uuid {
        self.server_secret
    }

    /// Number of datagrams dropped so far.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Number of live connections.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.by_secret.len()
    }

    /// Looks up a client by the secret it authenticates with.
    #[must_use]
    pub fn client_by_secret(&self, secret: &Uuid) -> Option<&Client> {
        self.by_secret.get(secret)
    }

    /// Looks up a client by its socket address.
    #[must_use]
    pub fn client_by_address(&self, address: &str) -> Option<&Client> {
        self.by_address
            .get(address)
            .and_then(|s| self.by_secret.get(s))
    }

    /// Registers a connection established over TCP.
    ///
    /// Mirrors `addConnection`: the secret is authoritative and the player id
    /// may be absent for a bare UDP session (`None`).
    pub fn add_connection(&mut self, secret: Uuid, address: String, player_id: Option<String>) {
        // One connection per secret and per address, like upstream's maps.
        //
        // The same secret may reconnect from a *new* address (a player changing
        // networks), so the secret's previous address entry has to be retired
        // explicitly; otherwise `by_address` keeps a stale pointer that still
        // resolves through `by_secret` and reports a connection at an address the
        // client no longer uses.
        if let Some(previous) = self.by_secret.remove(&secret) {
            self.by_address.remove(&previous.address);
        }
        // A second secret claiming an address this one already owns evicts the
        // previous owner entirely: the address maps to at most one connection.
        if let Some(old) = self.by_address.remove(&address)
            && old != secret
        {
            self.by_secret.remove(&old);
        }
        let client = Client {
            secret,
            address: address.clone(),
            player_id,
        };
        self.by_secret.insert(secret, client);
        self.by_address.insert(address, secret);
    }

    /// Removes a connection, returning it if it existed.
    pub fn remove_connection(&mut self, secret: &Uuid) -> Option<Client> {
        let client = self.by_secret.remove(secret)?;
        self.by_address.remove(&client.address);
        Some(client)
    }

    /// Handles one inbound datagram, returning what to send back.
    ///
    /// A datagram whose magic is wrong or whose secret is unknown is silently
    /// dropped (counted in [`Self::dropped`]) — the server never replies to
    /// traffic it does not recognise, which is what keeps a public UDP port from
    /// answering scanners.
    pub fn handle_datagram(&mut self, from: &str, data: &[u8]) -> Handled {
        // Only client->server ids are interesting here; `Server` direction is
        // what the server emits, so decoding must use `Server`.
        let packet = match self.codec.decode(data, PacketDirection::Server) {
            Ok(Some(packet)) => packet,
            // Wrong magic, unknown id, or not a client->server id.
            Ok(None) => {
                self.dropped = self.dropped.saturating_add(1);
                return Handled::nothing();
            }
            // Malformed body.
            Err(VoiceError::UnexpectedEof { .. } | VoiceError::TrailingBytes(_)) => {
                self.dropped = self.dropped.saturating_add(1);
                return Handled::nothing();
            }
            Err(_) => {
                self.dropped = self.dropped.saturating_add(1);
                return Handled::nothing();
            }
        };

        match packet {
            // A ping is answered with the same secret echoed back, along with
            // the server's own view of the endpoint. This is the only packet a
            // not-yet-connected client is allowed to send.
            UdpPacket::Ping(ping) => {
                // `server_ip` and `server_port` are two halves of one optional
                // endpoint: the wire format only writes it when *both* are present
                // (see `UdpPacket::write_body`), so they must be split correctly.
                // Putting the whole `ip:port` string in `server_ip` would either
                // duplicate the port or drop the endpoint entirely.
                let (server_ip, server_port) = match from.parse::<SocketAddr>() {
                    Ok(address) => (Some(address.ip().to_string()), Some(address.port())),
                    // A hostname (not an IP:port) still round-trips as the address
                    // half, with no port: the codec writes nothing, and the client
                    // learns the reply came from a plain host.
                    Err(_) => (Some(from.to_string()), None),
                };
                let reply = UdpPacket::Ping(plasmo_voice_core::wire::PingPacket {
                    time: ping.time,
                    server_ip,
                    server_port,
                });
                match self.codec.encode(&reply, self.server_secret, ping.time) {
                    Ok(data) => Handled {
                        outgoing: vec![Outgoing {
                            to: from.to_string(),
                            data,
                        }],
                        was_ping: true,
                    },
                    Err(_) => {
                        self.dropped = self.dropped.saturating_add(1);
                        Handled::nothing()
                    }
                }
            }

            // Audio from a connected player: relay to everyone else.
            UdpPacket::PlayerAudio(audio) => Handled {
                outgoing: self.relay(from, |client| {
                    self.codec
                        .encode(&UdpPacket::PlayerAudio(audio.clone()), client.secret, 0)
                        .unwrap_or_default()
                }),
                was_ping: false,
            },

            // Source / activation audio from the server itself never arrives
            // inbound; treat anything else as unknown traffic.
            _ => {
                self.dropped = self.dropped.saturating_add(1);
                Handled::nothing()
            }
        }
    }

    /// Encodes `packet` once per connected client other than `from`, using that
    /// client's own secret so only it can decrypt/accept the datagram.
    fn relay<F>(&self, from: &str, mut encode: F) -> Vec<Outgoing>
    where
        F: FnMut(&Client) -> Vec<u8>,
    {
        let mut out = Vec::new();
        for client in self.by_secret.values() {
            if client.address == from {
                continue;
            }
            let data = encode(client);
            if !data.is_empty() {
                out.push(Outgoing {
                    to: client.address.clone(),
                    data,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plasmo_voice_core::wire::{PingPacket, PlayerAudioPacket};

    const SERVER_SECRET: Uuid = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
    const CLIENT_SECRET: Uuid = Uuid::from_u128(0xaaaa_bbbb_cccc_dddd_eeee_ffff_0000_1111);

    fn codec() -> UdpCodec {
        UdpCodec::new()
    }

    #[test]
    fn pings_are_answered_with_the_server_secret() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let ping = UdpPacket::Ping(PingPacket {
            time: 42,
            server_ip: None,
            server_port: None,
        });
        let wire = codec().encode(&ping, CLIENT_SECRET, 42).expect("encode");

        let handled = server.handle_datagram("10.0.0.1:5000", &wire);
        assert!(handled.was_ping);
        assert_eq!(handled.outgoing.len(), 1);
        assert_eq!(handled.outgoing[0].to, "10.0.0.1:5000");
        assert_eq!(server.dropped(), 0);

        // The reply must be a valid ping under the *server* secret, sent in the
        // server->client direction (the id space is direction-specific).
        let decoded = codec()
            .decode(&handled.outgoing[0].data, PacketDirection::Server)
            .expect("decode ok")
            .expect("decoded");
        match decoded {
            UdpPacket::Ping(back) => {
                assert_eq!(back.time, 42);
                // `serverIp` and `serverPort` are separate wire fields (upstream
                // `PingPacket`), so the reply reports the address half and the
                // port half independently.
                assert_eq!(back.server_ip.as_deref(), Some("10.0.0.1"));
                assert_eq!(back.server_port, Some(5000));
            }
            other => panic!("expected ping, got {other:?}"),
        }
    }

    #[test]
    fn non_pv_traffic_is_dropped_without_a_reply() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let handled = server.handle_datagram("10.0.0.1:5000", b"hello there");
        assert!(handled.outgoing.is_empty());
        assert!(!handled.was_ping);
        assert_eq!(server.dropped(), 1);
    }

    #[test]
    fn truncated_ping_is_dropped() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        // Magic number then nothing else: wrong magic? no — correct magic but
        // the header is short, so decoding fails and nothing is sent.
        let mut wire = Vec::new();
        wire.extend_from_slice(&plasmo_voice_core::wire::UDP_MAGIC.to_be_bytes());
        wire.push(0x01);

        let handled = server.handle_datagram("10.0.0.1:5000", &wire);
        assert!(handled.outgoing.is_empty());
        assert_eq!(server.dropped(), 1);
    }

    #[test]
    fn connections_are_unique_per_secret_and_address() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        server.add_connection(
            CLIENT_SECRET,
            "1.2.3.4:10".to_string(),
            Some("alice".into()),
        );
        assert_eq!(server.connection_count(), 1);
        assert_eq!(
            server
                .client_by_secret(&CLIENT_SECRET)
                .map(|c| c.player_id.as_deref()),
            Some(Some("alice"))
        );

        // Re-adding the same secret from a new address moves it.
        server.add_connection(
            CLIENT_SECRET,
            "1.2.3.4:11".to_string(),
            Some("alice".into()),
        );
        assert_eq!(server.connection_count(), 1);
        assert!(server.client_by_address("1.2.3.4:10").is_none());
        assert!(server.client_by_address("1.2.3.4:11").is_some());

        // A second secret hijacking the same address evicts the first.
        let other = Uuid::from_u128(0x9999);
        server.add_connection(other, "1.2.3.4:11".to_string(), None);
        assert_eq!(server.connection_count(), 1);
        assert!(server.client_by_secret(&CLIENT_SECRET).is_none());
        assert_eq!(server.client_by_secret(&other).unwrap().player_id, None);

        assert_eq!(
            server.remove_connection(&other).map(|c| c.address),
            Some("1.2.3.4:11".to_string())
        );
        assert_eq!(server.connection_count(), 0);
        assert!(server.remove_connection(&other).is_none());
    }

    #[test]
    fn player_audio_is_relayed_to_other_clients_under_their_own_secret() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        server.add_connection(
            CLIENT_SECRET,
            "1.1.1.1:10".to_string(),
            Some("alice".into()),
        );
        let bob_secret = Uuid::from_u128(0xbbbb);
        server.add_connection(bob_secret, "2.2.2.2:20".to_string(), Some("bob".into()));

        let audio = PlayerAudioPacket {
            sequence_number: 7,
            data: vec![1, 2, 3, 4],
            activation_id: Uuid::from_u128(0x5),
            distance: 16,
            stereo: false,
        };
        let wire = codec()
            .encode(&UdpPacket::PlayerAudio(audio.clone()), CLIENT_SECRET, 0)
            .expect("encode");

        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert_eq!(handled.outgoing.len(), 1, "only bob should receive it");
        assert_eq!(handled.outgoing[0].to, "2.2.2.2:20");

        // Decoding with bob's secret is required; alice's must not match.
        assert!(
            codec()
                .decode_header(&handled.outgoing[0].data)
                .expect("header")
                .is_some()
        );
        // PlayerAudio is the SERVER direction id (upstream 0x02), because the
        // server is the one relaying it onward to other clients.
        let decoded = codec()
            .decode(&handled.outgoing[0].data, PacketDirection::Server)
            .expect("decode")
            .expect("packet");
        assert_eq!(decoded, UdpPacket::PlayerAudio(audio));
    }

    #[test]
    fn server_direction_ids_are_not_accepted_inbound() {
        // SourceAudio is CLIENT-direction, i.e. the server *sends* it; a client
        // sending one must be ignored.
        let mut server = VoiceServer::new(SERVER_SECRET);
        let packet = UdpPacket::SourceAudio(plasmo_voice_core::wire::SourceAudioPacket {
            sequence_number: 1,
            data: vec![9],
            source_id: Uuid::from_u128(1),
            source_state: 0,
            distance: 0,
        });
        let wire = codec().encode(&packet, CLIENT_SECRET, 0).expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(handled.outgoing.is_empty());
        assert_eq!(server.dropped(), 1);
    }
}
