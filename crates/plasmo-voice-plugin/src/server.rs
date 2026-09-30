//! The voice server's protocol logic, decoupled from the socket.
//!
//! [`VoiceServer`] owns the UDP *semantics* (packet decoding, per-connection
//! secrets, audio fan-out) and never touches a socket itself. `lib.rs` drives it
//! from the plugin's `std::net::UdpSocket`, while tests drive it directly with
//! synthetic datagrams — so the interesting logic is covered on the host target
//! even though the real server runs on `wasm32-wasip2`.

use std::collections::HashMap;

use plasmo_voice_core::wire::PingPacket;
use plasmo_voice_core::{PacketDirection, UdpCodec, UdpPacket};
use uuid::Uuid;

/// A client known to the voice server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// The secret UUID carried in every UDP frame header from this client.
    pub secret: Uuid,
    /// `host:port` the client is speaking from.
    pub address: String,
    /// The player this connection belongs to, when bound over the control plane.
    pub player_id: Option<String>,
    /// Wall-clock ms of the last datagram seen from this client
    /// (upstream `UdpServerConnection.getLastReceivedPacketTimestamp`).
    pub last_received_ms: u64,
    /// Wall-clock ms after which another keep-alive ping may be sent
    /// (upstream `getSentKeepAlive`, which likewise stores a *deadline*).
    pub next_keep_alive_ms: u64,
}

/// A player announced by the control plane, with the secret it must speak with.
///
/// Mirrors upstream's `secretByPlayerId` / `playerIdBySecret` pair:
/// `VoiceUdpServerConnectionManager.getSecretByPlayerId` mints one secret per
/// player, ships it to the client inside a clientbound `ConnectionPacket`, and
/// `NettyPacketHandler` then refuses every datagram whose secret is not in that
/// map. Without a registration a connection can never come into existence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredPlayer {
    /// The Minecraft player UUID.
    pub player_id: Uuid,
    /// The secret the client authenticates UDP traffic with.
    pub secret: Uuid,
    /// Display name, when the control plane supplied one.
    pub name: Option<String>,
}

/// Milliseconds between keep-alive pings to an idle connection.
///
/// Upstream `NettyUdpKeepAlive.tick` pings when `now - sentKeepAlive >= 1_000L`
/// and then pushes the deadline out by 1.5-3s of jitter.
pub const KEEP_ALIVE_INTERVAL_MS: u64 = 1_000;

/// Drop a connection that has sent nothing for this long.
///
/// Upstream `VoiceServerConfig.keepAliveTimeoutMs` defaults to `15_000`.
pub const KEEP_ALIVE_TIMEOUT_MS: u64 = 15_000;

/// Milliseconds since the Unix epoch, saturating.
///
/// Only keep-alive bookkeeping uses the clock, so an unreadable (or pre-epoch)
/// clock degrades to `0` instead of panicking.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |delta| {
            u64::try_from(delta.as_millis()).unwrap_or(u64::MAX)
        })
}

/// One outgoing datagram produced by [`VoiceServer::handle_datagram`] or
/// [`VoiceServer::keep_alive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub to: String,
    pub data: Vec<u8>,
}

/// Decides what the server does with an inbound datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handled {
    /// Datagrams to send back (forwarded audio; pings are never answered).
    pub outgoing: Vec<Outgoing>,
    /// Set when the packet was a ping — a registration request or keep-alive ack.
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
/// also maps to at most one connection keyed by address, and the control plane
/// maps a player to the secret that player's client will use.
#[derive(Debug)]
pub struct VoiceServer {
    codec: UdpCodec,
    /// secret -> client
    by_secret: HashMap<Uuid, Client>,
    /// address -> secret
    by_address: HashMap<String, Uuid>,
    /// player id -> registration (upstream `secretByPlayerId`).
    registered: HashMap<Uuid, RegisteredPlayer>,
    /// secret -> player id (upstream `playerIdBySecret`).
    player_by_secret: HashMap<Uuid, Uuid>,
    /// Folded into minted secrets so two players registered in the same
    /// millisecond cannot collide.
    secret_counter: u64,
    /// The server-wide secret advertised in the IPC `handshake` reply.
    server_secret: Uuid,
    /// Datagrams dropped before decoding (wrong magic / bad header / unknown
    /// secret) or rejected after decoding (unknown id / wrong direction).
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
            registered: HashMap::new(),
            player_by_secret: HashMap::new(),
            secret_counter: 0,
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

    /// Registers a live UDP connection.
    ///
    /// Mirrors `addConnection`: the secret is authoritative and the player id is
    /// the one the control plane registered for that secret.
    ///
    /// Deliberately **crate-private**: [`VoiceServer::handle_datagram`] is the only
    /// production caller, and it only reaches this point after the secret matched the
    /// control-plane registry. Exposing it is how the connection table would end up
    /// trusting a secret that nobody registered.
    pub(crate) fn add_connection(
        &mut self,
        secret: Uuid,
        address: String,
        player_id: Option<String>,
    ) {
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
        let now = now_ms();
        let client = Client {
            secret,
            address: address.clone(),
            player_id,
            last_received_ms: now,
            // Upstream's `sentKeepAlive` starts unset, so the first sweep pings
            // the new connection immediately; that first ping is also what makes
            // a real client consider itself connected.
            next_keep_alive_ms: now,
        };
        self.by_secret.insert(secret, client);
        self.by_address.insert(address, secret);
    }

    /// Removes a connection, returning it if it existed.
    ///
    /// Crate-private for the same reason as [`VoiceServer::add_connection`]: the
    /// callers are the disconnect path and the keep-alive sweep.
    pub(crate) fn remove_connection(&mut self, secret: &Uuid) -> Option<Client> {
        let client = self.by_secret.remove(secret)?;
        self.by_address.remove(&client.address);
        Some(client)
    }

    /// Follows a client that moved to a different UDP address.
    ///
    /// Mirrors upstream's `setRemoteAddress`, which the packet handler calls when
    /// a datagram's sender differs from the address recorded for that secret.
    fn readdress(&mut self, secret: Uuid, address: &str) {
        let Some(client) = self.by_secret.get(&secret) else {
            return;
        };
        if client.address == address {
            return;
        }
        let previous = client.address.clone();
        self.by_address.remove(&previous);
        if let Some(client) = self.by_secret.get_mut(&secret) {
            client.address = address.to_string();
        }
        self.by_address.insert(address.to_string(), secret);
    }

    /// Registers `player_id` and returns the secret its client must speak with.
    ///
    /// The secret is minted once per player and then reused, like upstream's
    /// `getSecretByPlayerId`, because a client may rebuild its UDP socket without
    /// a new control-plane round trip. `name` only feeds logs and the status
    /// reply.
    pub fn register_player(&mut self, player_id: Uuid, name: Option<String>) -> Uuid {
        if let Some(existing) = self.registered.get_mut(&player_id) {
            if name.is_some() {
                existing.name = name;
            }
            return existing.secret;
        }

        self.secret_counter = self.secret_counter.wrapping_add(1);
        let seed = (player_id.as_u128() as u64)
            ^ self.secret_counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (self.server_secret.as_u128() as u64);
        let secret = crate::state::VoiceServerState::generate_secret_uuid(seed);

        self.registered.insert(
            player_id,
            RegisteredPlayer {
                player_id,
                secret,
                name,
            },
        );
        self.player_by_secret.insert(secret, player_id);
        secret
    }

    /// Forgets a player, dropping any live connection it owned.
    ///
    /// Mirrors upstream's disconnect path: the connection leaves both maps and the
    /// secret stops being usable, so a client that keeps talking after a
    /// disconnect is dropped instead of relayed.
    pub fn unregister_player(&mut self, player_id: &Uuid) -> Option<Uuid> {
        let registered = self.registered.remove(player_id)?;
        self.player_by_secret.remove(&registered.secret);
        self.remove_connection(&registered.secret);
        Some(registered.secret)
    }

    /// The secret registered for `player_id`, if any.
    #[must_use]
    pub fn secret_for_player(&self, player_id: &Uuid) -> Option<Uuid> {
        self.registered.get(player_id).map(|entry| entry.secret)
    }

    /// The player a secret belongs to, if any (upstream `getPlayerIdBySecret`).
    #[must_use]
    pub fn player_for_secret(&self, secret: &Uuid) -> Option<Uuid> {
        self.player_by_secret.get(secret).copied()
    }

    /// The name the control plane gave for `player_id`, if any.
    #[must_use]
    pub fn player_name(&self, player_id: &Uuid) -> Option<&str> {
        self.registered
            .get(player_id)
            .and_then(|entry| entry.name.as_deref())
    }

    /// Number of control-plane registrations (not UDP connections).
    #[must_use]
    pub fn registered_player_count(&self) -> usize {
        self.registered.len()
    }

    /// All registrations, ordered by player id so the status reply is stable.
    #[must_use]
    pub fn registered_players(&self) -> Vec<RegisteredPlayer> {
        let mut players: Vec<_> = self.registered.values().cloned().collect();
        players.sort_by_key(|entry| entry.player_id);
        players
    }

    /// Pings idle connections and retires the silent ones.
    ///
    /// Mirrors `NettyUdpKeepAlive.tick`: a connection that has sent nothing for
    /// `timeout_ms` is removed, and every other connection gets an empty
    /// `PingPacket` at most once per second. Upstream spreads the pings with
    /// 1.5-3s of jitter; deriving that jitter from the secret keeps the spread
    /// without needing an RNG in the guest. Returns the datagrams to send.
    pub fn keep_alive(&mut self, now: u64, timeout_ms: u64) -> Vec<Outgoing> {
        let mut expired = Vec::new();
        let mut due = Vec::new();

        for client in self.by_secret.values_mut() {
            if now.saturating_sub(client.last_received_ms) > timeout_ms {
                expired.push(client.secret);
            } else if now >= client.next_keep_alive_ms {
                let jitter = 1_500 + (client.secret.as_u128() as u64 % 1_500);
                client.next_keep_alive_ms = now.saturating_add(jitter);
                due.push(client.secret);
            }
        }

        for secret in expired {
            self.remove_connection(&secret);
        }

        due.into_iter()
            .filter_map(|secret| {
                let client = self.by_secret.get(&secret)?;
                let ping = UdpPacket::Ping(PingPacket::new(None, 0));
                let timestamp = i64::try_from(now).unwrap_or(i64::MAX);
                let data = self.codec.encode(&ping, secret, timestamp).ok()?;
                Some(Outgoing {
                    to: client.address.clone(),
                    data,
                })
            })
            .collect()
    }

    /// Handles one inbound datagram, returning what to send back.
    ///
    /// Mirrors upstream `NettyPacketHandler`: **the header secret decides
    /// everything**. A datagram is dropped (counted in [`Self::dropped`]) when its
    /// magic is wrong, its header is short, its id is unknown or registered for
    /// the other direction, its body is malformed — or when its secret belongs to
    /// neither a live connection nor a control-plane registration. The server
    /// never answers traffic it does not recognise, which is what keeps a public
    /// UDP port from relaying a stranger's audio.
    pub fn handle_datagram(&mut self, from: &str, data: &[u8]) -> Handled {
        let envelope = match self.codec.decode_header(data) {
            Ok(Some(envelope)) => envelope,
            // Wrong magic (`Ok(None)`) or a truncated header (`Err`).
            Ok(None) | Err(_) => {
                self.dropped = self.dropped.saturating_add(1);
                return Handled::nothing();
            }
        };
        let secret = envelope.secret;

        if self.by_secret.contains_key(&secret) {
            // A client that roamed to another address is followed, like
            // upstream's `setRemoteAddress`.
            self.readdress(secret, from);
        } else if let Some(player_id) = self.player_by_secret.get(&secret).copied() {
            // First datagram from a *registered* player: this is where the UDP
            // connection is born (`NettyPacketHandler` creates and adds it, then
            // sends the config/player-list burst over the control plane).
            self.add_connection(secret, from.to_string(), Some(player_id.to_string()));
        } else {
            self.dropped = self.dropped.saturating_add(1);
            return Handled::nothing();
        }

        let packet = match envelope.decode_packet(PacketDirection::Server) {
            Ok(Some(packet)) => packet,
            // Unknown id, or an id registered for the server->client direction.
            Ok(None) | Err(_) => {
                self.dropped = self.dropped.saturating_add(1);
                return Handled::nothing();
            }
        };

        // Upstream stamps `lastReceivedPacketTimestamp` once the packet decodes.
        let now = now_ms();
        if let Some(client) = self.by_secret.get_mut(&secret) {
            client.last_received_ms = now;
        }

        match packet {
            // Upstream never answers a ping. A client pings once per second until
            // it hears from the server, and it answers *any* inbound ping with
            // another ping — so echoing pings back would make the two sides
            // ping-pong without bound. The server sends its own keep-alive pings
            // instead (see [`VoiceServer::keep_alive`]); a client ping is a
            // registration request and a keep-alive ack, and reaching this line
            // means it did its job.
            UdpPacket::Ping(_) => Handled {
                outgoing: Vec::new(),
                was_ping: true,
            },

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

    /// Registers a player over the control plane, as the IPC handler does.
    fn register(server: &mut VoiceServer, player: u128) -> Uuid {
        let player_id = Uuid::from_u128(player);
        server.register_player(player_id, Some(format!("player-{player:x}")))
    }

    fn empty_ping() -> UdpPacket {
        UdpPacket::Ping(PingPacket {
            time: 1,
            server_ip: None,
            server_port: None,
        })
    }

    #[test]
    fn a_registered_players_ping_creates_the_connection_and_is_not_echoed() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let secret = register(&mut server, 0xa11ce);

        let wire = codec().encode(&empty_ping(), secret, 42).expect("encode");
        let handled = server.handle_datagram("10.0.0.1:5000", &wire);

        assert!(handled.was_ping);
        // Upstream `NettyPacketHandler` registers the connection and answers
        // nothing at all: the client's ping *is* the registration request, and
        // echoing a ping back would make a real client answer forever.
        assert!(handled.outgoing.is_empty());
        assert_eq!(server.dropped(), 0);
        assert_eq!(server.connection_count(), 1);

        let client = server.client_by_secret(&secret).expect("connection");
        assert_eq!(client.address, "10.0.0.1:5000");
        let player_id = server.player_for_secret(&secret).expect("registered");
        assert_eq!(
            client.player_id.as_deref(),
            Some(player_id.to_string().as_str())
        );
        assert_eq!(
            server.client_by_address("10.0.0.1:5000").map(|c| c.secret),
            Some(secret)
        );
    }

    #[test]
    fn a_client_that_roams_is_followed_to_its_new_address() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let secret = register(&mut server, 0xb0b);
        let wire = codec().encode(&empty_ping(), secret, 1).expect("encode");

        server.handle_datagram("10.0.0.1:5000", &wire);
        server.handle_datagram("10.0.0.9:6000", &wire);

        assert_eq!(server.connection_count(), 1, "one secret, one connection");
        assert_eq!(
            server
                .client_by_secret(&secret)
                .expect("connection")
                .address,
            "10.0.0.9:6000",
            "the connection follows the client, like upstream's setRemoteAddress"
        );
        assert!(server.client_by_address("10.0.0.1:5000").is_none());
    }

    #[test]
    fn datagrams_with_an_unknown_secret_are_dropped() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        // Perfectly well-formed traffic — but nobody registered this secret, so
        // upstream's packet handler returns before creating a connection.
        let wire = codec()
            .encode(&empty_ping(), Uuid::from_u128(0xdead), 1)
            .expect("encode");

        let handled = server.handle_datagram("10.0.0.1:5000", &wire);
        assert!(handled.outgoing.is_empty());
        assert!(
            !handled.was_ping,
            "an unregistered secret never reaches the packet body"
        );
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.dropped(), 1);
    }

    #[test]
    fn audio_from_an_unregistered_sender_is_never_relayed() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let bob_secret = register(&mut server, 0xb0b);
        server.add_connection(bob_secret, "2.2.2.2:20".to_string(), Some("bob".into()));

        let audio = PlayerAudioPacket {
            sequence_number: 1,
            data: vec![1],
            activation_id: Uuid::from_u128(5),
            distance: 16,
            stereo: false,
        };
        let wire = codec()
            .encode(&UdpPacket::PlayerAudio(audio), CLIENT_SECRET, 0)
            .expect("encode");

        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(
            handled.outgoing.is_empty(),
            "a stranger's audio must never reach a connected client"
        );
        assert_eq!(server.dropped(), 1);
    }

    #[test]
    fn unregistering_a_player_drops_its_connection_and_its_secret() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let player = Uuid::from_u128(0xa11ce);
        let secret = server.register_player(player, Some("alice".into()));
        server.add_connection(secret, "1.1.1.1:10".to_string(), Some(player.to_string()));

        assert_eq!(server.unregister_player(&player), Some(secret));
        assert_eq!(server.connection_count(), 0);
        assert!(server.registered_players().is_empty());
        assert!(server.player_for_secret(&secret).is_none());
        assert!(server.unregister_player(&player).is_none());

        // Disconnected means disconnected: the secret stops being a way in.
        let wire = codec().encode(&empty_ping(), secret, 1).expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(handled.outgoing.is_empty());
        assert_eq!(server.dropped(), 1);
    }

    #[test]
    fn registering_a_player_twice_reuses_the_secret() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let player = Uuid::from_u128(0xa11ce);

        let first = server.register_player(player, None);
        assert_eq!(server.secret_for_player(&player), Some(first));
        assert_eq!(server.register_player(player, Some("alice".into())), first);
        assert_eq!(server.registered_player_count(), 1);
        assert_eq!(
            server.player_name(&player),
            Some("alice"),
            "a later name fills in (or replaces) the display name"
        );
    }

    #[test]
    fn distinct_players_get_distinct_secrets() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let alice = register(&mut server, 0xa11ce);
        let bob = register(&mut server, 0xb0b);

        assert_ne!(alice, bob);
        assert_eq!(server.registered_player_count(), 2);
        assert_eq!(server.registered_players().len(), 2);
    }

    #[test]
    fn keep_alive_pings_idle_connections_and_retires_silent_ones() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let secret = register(&mut server, 0xc0ffee);
        server.add_connection(secret, "1.1.1.1:10".to_string(), None);
        let now = now_ms();

        // Upstream's `sentKeepAlive` starts unset, so the first sweep pings at
        // once — and that first ping is what makes a real client "connected".
        let first = server.keep_alive(now, KEEP_ALIVE_TIMEOUT_MS);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].to, "1.1.1.1:10");
        match codec()
            .decode(&first[0].data, PacketDirection::Server)
            .expect("decode ok")
            .expect("packet")
        {
            UdpPacket::Ping(ping) => assert!(
                ping.server_ip.is_none() && ping.server_port.is_none(),
                "a keep-alive ping carries no endpoint, like `new PingPacket()`"
            ),
            other => panic!("expected ping, got {other:?}"),
        }
        // The frame is addressed to the connection's own secret, exactly like
        // upstream's `NettyUdpServerConnection.sendPacket`.
        assert_eq!(
            codec()
                .decode_header(&first[0].data)
                .expect("header")
                .expect("envelope")
                .secret,
            secret
        );

        // Not again inside the jittered interval...
        assert!(
            server
                .keep_alive(now + 10, KEEP_ALIVE_TIMEOUT_MS)
                .is_empty()
        );

        // ...but a connection silent past the timeout is retired.
        assert!(
            server
                .keep_alive(now + KEEP_ALIVE_TIMEOUT_MS + 1, KEEP_ALIVE_TIMEOUT_MS)
                .is_empty()
        );
        assert_eq!(server.connection_count(), 0);
    }

    #[test]
    fn keep_alive_does_not_expire_a_client_that_keeps_talking() {
        let mut server = VoiceServer::new(SERVER_SECRET);
        let secret = register(&mut server, 0xc0ffee);
        server.add_connection(secret, "1.1.1.1:10".to_string(), None);

        let wire = codec().encode(&empty_ping(), secret, 1).expect("encode");
        server.handle_datagram("1.1.1.1:10", &wire);

        // The datagram refreshed the connection's timestamp, so a sweep just past
        // the old deadline still finds it alive.
        let _ = server.keep_alive(now_ms() + 1, KEEP_ALIVE_TIMEOUT_MS);
        assert_eq!(server.connection_count(), 1);
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
        // The control plane registers both players and their first datagrams
        // create the UDP connections — the full path a running server takes.
        let alice_secret = register(&mut server, 0xa11ce);
        let bob_secret = register(&mut server, 0xb0b);
        let alice_ping = codec()
            .encode(&empty_ping(), alice_secret, 1)
            .expect("encode");
        server.handle_datagram("1.1.1.1:10", &alice_ping);
        let bob_ping = codec()
            .encode(&empty_ping(), bob_secret, 1)
            .expect("encode");
        server.handle_datagram("2.2.2.2:20", &bob_ping);
        assert_eq!(server.connection_count(), 2);

        let audio = PlayerAudioPacket {
            sequence_number: 7,
            data: vec![1, 2, 3, 4],
            activation_id: Uuid::from_u128(0x5),
            distance: 16,
            stereo: false,
        };
        let wire = codec()
            .encode(&UdpPacket::PlayerAudio(audio.clone()), alice_secret, 0)
            .expect("encode");

        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert_eq!(handled.outgoing.len(), 1, "only bob should receive it");
        assert_eq!(handled.outgoing[0].to, "2.2.2.2:20");
        // The relayed frame is addressed to *bob's* secret: alice's must not
        // match, or every client would have to guess the sender's key.
        assert_eq!(
            codec()
                .decode_header(&handled.outgoing[0].data)
                .expect("header")
                .expect("envelope")
                .secret,
            bob_secret
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
        // Registered, so the datagram passes the secret check and the *direction*
        // check is what rejects it.
        let secret = register(&mut server, 0xa11ce);
        let packet = UdpPacket::SourceAudio(plasmo_voice_core::wire::SourceAudioPacket {
            sequence_number: 1,
            data: vec![9],
            source_id: Uuid::from_u128(1),
            source_state: 0,
            distance: 0,
        });
        let wire = codec().encode(&packet, secret, 0).expect("encode");

        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(handled.outgoing.is_empty());
        assert_eq!(server.connection_count(), 1, "the secret was accepted");
        assert_eq!(server.dropped(), 1);
    }
}
