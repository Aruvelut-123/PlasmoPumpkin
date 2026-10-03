//! The voice server's protocol logic, decoupled from the socket.
//!
//! [`VoiceServer`] owns the UDP *semantics* (packet decoding, per-connection
//! secrets, audio fan-out) and never touches a socket itself. `lib.rs` drives it
//! from the plugin's `std::net::UdpSocket`, while tests drive it directly with
//! synthetic datagrams — so the interesting logic is covered on the host target
//! even though the real server runs on `wasm32-wasip2`.

use std::collections::{HashMap, HashSet};

use plasmo_voice_core::data::{
    CodecInfo, PlayerSourceInfo, SelfSourceInfo, SourceInfo, SourceInfoBase, VoicePlayerInfo,
};
use plasmo_voice_core::wire::{
    PingPacket, PlayerInfoUpdatePacket, SelfAudioInfoPacket, SourceAudioPacket, TcpPacket,
};
use plasmo_voice_core::{PacketDirection, UdpCodec, UdpPacket};
use uuid::Uuid;

use crate::config::{ServerConfig, calculate_allowed_distance};
use crate::control::{ControlPlane, Outbound};

/// A client known to the voice server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// The secret UUID carried in every UDP frame header from this client.
    pub secret: Uuid,
    /// `host:port` the client is speaking from.
    pub address: String,
    /// The endpoint the client reported seeing this server at, from the first
    /// `PingPacket` it sent (upstream `UdpConnection.getConnectionAddress`).
    ///
    /// Informational only: it is **not** where datagrams are sent. Upstream keeps this
    /// and `remoteAddress` in two separate fields, and only `remoteAddress` — the
    /// address a datagram actually arrived from — is ever a send target.
    pub connection_address: Option<String>,
    /// The player this connection belongs to, when bound over the control plane.
    pub player_id: Option<Uuid>,
    /// The player's audio source id (upstream `VoiceServerPlayerSource.id`, a fresh
    /// random UUID per player). Listeners learn it from a `SourceInfoPacket` and see it
    /// on every relayed `SourceAudioPacket`, so it must be stable for the session.
    pub source_id: Uuid,
    /// The source's state byte (upstream `BaseServerAudioSource.state`): starts at 1 and
    /// moves by +1 on name/icon changes and +10 on stereo changes, wrapping at
    /// `i8::MAX`. A client drops frames whose state is more than 10 away from its cached
    /// value, so this must only change when upstream would change it.
    pub source_state: i8,
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
///
/// The player's *state* lives here rather than on the connection because upstream keeps
/// it on the player (`BaseVoicePlayer`), and it outlives a UDP reconnect.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredPlayer {
    /// The Minecraft player UUID.
    pub player_id: Uuid,
    /// The secret the client authenticates UDP traffic with.
    pub secret: Uuid,
    /// Display name, when the control plane supplied one.
    pub name: Option<String>,
    /// The audio source id this player's voice is published under.
    pub source_id: Uuid,
    /// The client's RSA public key (`PlayerInfoPacket.public_key`), X.509
    /// `SubjectPublicKeyInfo` DER — the encoding a Java client sends, because it
    /// reads the bytes out of `X509EncodedKeySpec(publicKey.getEncoded())`.
    ///
    /// Stored so the *registration burst* can wrap the server-wide AES key for
    /// exactly this player (`ConfigPacket.encryption`, upstream
    /// `VoiceTcpServerConnectionManager.sendConfigInfo`). A client that sent no
    /// key — or sent one this server cannot parse — gets an unencrypted config
    /// and plaintext audio, matching upstream's `encryption == null` path.
    pub public_key: Option<Vec<u8>>,
    /// Server-side mute (`VoiceMuteManager`); false until a mute manager exists.
    pub muted: bool,
    /// The client reported voice chat disabled (clientbound `PlayerStatePacket`).
    pub voice_disabled: bool,
    /// The client reported its microphone muted.
    pub microphone_muted: bool,
    /// Distances the client asked for, per activation (`PlayerActivationDistancesPacket`).
    pub activation_distances: Vec<(Uuid, i32)>,
    /// Last known position, from a move event or a per-tick refresh.
    pub position: Option<(f64, f64, f64)>,
    /// Last known world (`world.get-id`), so the relay can refuse cross-world audio.
    pub world: Option<String>,
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

/// How many times each "this datagram is not ours" warning is emitted before the
/// message drops to `debug`.
///
/// A misconfigured client retries forever and a public port gets scanned, so the
/// warning has to be loud once and cheap thereafter — but it must exist, because
/// "packets arrive and are silently ignored" is otherwise invisible.
const UNKNOWN_SECRET_WARNINGS: u64 = 3;

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

/// A stable seed derived from a secret and a salt.
///
/// Used where the value must be *reproducible* for a given secret rather than
/// unpredictable, so a test can assert it.
fn seed_for(secret: &Uuid, salt: u64) -> u64 {
    let wide = secret.as_u128();
    (wide as u64) ^ ((wide >> 64) as u64) ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)
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
    /// Datagrams to send back (relayed audio and self-info; pings are never answered).
    pub outgoing: Vec<Outgoing>,
    /// Control-plane messages to deliver over the `plasmo:voice` channel. The very first
    /// datagram of a session produces the registration burst here.
    pub control: Vec<Outbound>,
    /// Set when the packet was a ping — a registration request or keep-alive ack.
    pub was_ping: bool,
}

impl Handled {
    fn nothing() -> Self {
        Self {
            outgoing: Vec::new(),
            control: Vec::new(),
            was_ping: false,
        }
    }
}

/// What one keep-alive sweep produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sweep {
    /// Keep-alive pings to send.
    pub outgoing: Vec<Outgoing>,
    /// Disconnect broadcasts for the connections the sweep retired.
    pub control: Vec<Outbound>,
    /// Players whose *connection* timed out but whose registration survives.
    ///
    /// Upstream re-asks these for their info (`NettyUdpKeepAlive:48` calls
    /// `requestPlayerInfo` right after `removeConnection(..., TIMED_OUT)`), which restarts
    /// the handshake and hands the client a fresh `ConnectionPacket`. That is how a client
    /// whose UDP path broke recovers without rejoining, so the runtime must see the list.
    pub timed_out_players: Vec<Uuid>,
}

/// The Plasmo Voice UDP server.
///
/// Connections are keyed by secret (`connectionBySecret` upstream); a client
/// also maps to at most one connection keyed by address, and the control plane
/// maps a player to the secret that player's client will use.
#[derive(Debug)]
pub struct VoiceServer {
    codec: UdpCodec,
    /// The `plasmo:voice` control plane: secrets, config and the packets a client
    /// exchanges with us over the plugin-message channel.
    control: ControlPlane,
    /// secret -> client
    by_secret: HashMap<Uuid, Client>,
    /// address -> secret
    by_address: HashMap<String, Uuid>,
    /// player id -> registration (upstream `secretByPlayerId`).
    registered: HashMap<Uuid, RegisteredPlayer>,
    /// secret -> player id (upstream `playerIdBySecret`).
    player_by_secret: HashMap<Uuid, Uuid>,
    /// (viewer, target) pairs where `viewer` cannot see `target` on the host.
    ///
    /// Synced from the tick pump: the host owns the truth (its `hidePlayer` /
    /// `showPlayer` state), so the guest mirrors `canSee` on a schedule. The
    /// relay mutes a pair in **both** directions — a vanished player is fully
    /// gone from the voice world, not just inaudible.
    hidden_players: HashSet<(Uuid, Uuid)>,
    /// Folded into minted secrets so two players registered in the same
    /// millisecond cannot collide.
    secret_counter: u64,
    /// The server-wide secret advertised in the IPC `handshake` reply.
    server_secret: Uuid,
    /// The server-wide AES key, wrapped per client into `ConfigPacket.encryption`.
    ///
    /// Persisted across reloads (see `state.aes_key`) so clients that are already
    /// connected keep a working cipher.
    aes_key: crate::crypto::AesKey,
    /// Datagrams dropped before decoding (wrong magic / bad header / unknown
    /// secret) or rejected after decoding (unknown id / wrong direction).
    dropped: u64,
    /// How many of those were for a secret this server never issued. Kept apart
    /// from the total because it is the one drop that proves a client reached
    /// the socket (see the warning in [`VoiceServer::handle_datagram`]).
    unknown_secret_drops: u64,
    /// How many datagrams were not Plasmo Voice packets at all.
    malformed_drops: u64,
    /// Payloads that arrived on the `plasmo:voice` channel and did not decode.
    undecodable_payloads: u64,
}

impl VoiceServer {
    /// Creates a server whose advertised secret is `server_secret` and whose
    /// clients share the AES key `aes_key`.
    ///
    /// The server id in the clientbound `ConfigPacket` is the same secret: it identifies
    /// this voice server to the client's own config store. The AES key is the one
    /// persisted value the data plane needs across reloads.
    #[must_use]
    pub fn new(server_secret: Uuid, aes_key: crate::crypto::AesKey) -> Self {
        Self::new_with_config(server_secret, aes_key, ServerConfig::new(server_secret))
    }

    /// Like [`VoiceServer::new`], but carrying an operator-overridden config.
    #[must_use]
    pub fn new_with_config(
        server_secret: Uuid,
        aes_key: crate::crypto::AesKey,
        config: ServerConfig,
    ) -> Self {
        Self {
            codec: UdpCodec::new(),
            control: ControlPlane::with_config(config),
            by_secret: HashMap::new(),
            by_address: HashMap::new(),
            registered: HashMap::new(),
            player_by_secret: HashMap::new(),
            hidden_players: HashSet::new(),
            secret_counter: 0,
            server_secret,
            aes_key,
            dropped: 0,
            unknown_secret_drops: 0,
            malformed_drops: 0,
            undecodable_payloads: 0,
        }
    }

    /// Records a payload on the `plasmo:voice` channel that the codec refused.
    ///
    /// Upstream treats all three causes (unknown id, wrong direction, malformed body) as
    /// silence, and so do we — but a *client* whose payloads consistently fail to decode
    /// is a bug on this side, so the first few are reported loudly enough to be seen from
    /// a server log while a client sits there looking connected.
    pub fn note_undecodable_payload(&mut self, player: Uuid, bytes: usize) {
        self.undecodable_payloads = self.undecodable_payloads.saturating_add(1);
        if self.undecodable_payloads <= UNKNOWN_SECRET_WARNINGS {
            tracing::warn!(
                %player,
                bytes,
                total = self.undecodable_payloads,
                "a client ({player}) sent a {bytes}-byte voice payload that does not decode \
                 ({} so far)",
                self.undecodable_payloads
            );
        } else {
            tracing::debug!(%player, bytes, "undecodable voice payload");
        }
    }

    /// The control plane (config + packet state machine).
    #[must_use]
    pub const fn control(&self) -> &ControlPlane {
        &self.control
    }

    /// The server's voice configuration.
    #[must_use]
    pub fn config(&self) -> &ServerConfig {
        self.control.config()
    }

    /// Handles one `plasmo:voice` control payload, returning the messages to deliver.
    ///
    /// The control plane is stateless, so this clones it to break the borrow overlap:
    /// `ControlPlane::handle` needs the plane *and* the server at once. The clone is a
    /// `Uuid` plus a stateless codec, which is far cheaper than a second source of truth
    /// would be.
    pub fn handle_control(
        &mut self,
        player: Uuid,
        name: &str,
        ip: &str,
        port: u16,
        data: &[u8],
    ) -> Vec<Outbound> {
        let control = self.control.clone();
        control.handle(self, player, name, ip, port, data)
    }

    /// Forgets a player who left the Minecraft server, announcing it to the others.
    pub fn control_disconnect(&mut self, player: Uuid) -> Vec<Outbound> {
        let control = self.control.clone();
        control.disconnect(self, player)
    }

    /// The encoded `PlayerInfoRequestPacket` (phase 1 of the handshake).
    #[must_use]
    pub fn request_player_info(&self) -> Vec<u8> {
        self.control.request_player_info()
    }

    /// The secret this server advertises to clients.
    #[must_use]
    pub const fn server_secret(&self) -> Uuid {
        self.server_secret
    }

    /// The server-wide AES key shared by every client.
    ///
    /// Only the control plane reads it, when it wraps the key per player for
    /// `ConfigPacket.encryption`.
    #[must_use]
    pub const fn aes_key(&self) -> &crate::crypto::AesKey {
        &self.aes_key
    }

    /// Stores the client's RSA public key, for the config packet that follows.
    ///
    /// Re-registering a player (a client rebuilds its UDP socket without a new
    /// control-plane round trip) keeps the earlier key — a client does not resend
    /// `PlayerInfoPacket` on a reconnect.
    pub fn set_player_public_key(&mut self, player_id: Uuid, public_key: &[u8]) {
        if let Some(registered) = self.registered.get_mut(&player_id) {
            registered.public_key = Some(public_key.to_vec());
        }
    }

    /// The client's RSA public key, if it sent one.
    #[must_use]
    pub fn player_public_key(&self, player_id: &Uuid) -> Option<&[u8]> {
        self.registered
            .get(player_id)
            .and_then(|registered| registered.public_key.as_deref())
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
        player_id: Option<Uuid>,
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
        // The source id belongs to the *player* (upstream caches one
        // `VoiceServerPlayerSource` per player UUID forever), so a reconnect keeps the
        // id its listeners already know. A connection with no registration of its own
        // can only come from a test, and gets a fresh id.
        let source_id = player_id
            .and_then(|id| self.registered.get(&id))
            .map_or_else(
                || crate::state::VoiceServerState::generate_secret_uuid(seed_for(&secret, 0x51)),
                |p| p.source_id,
            );
        let now = now_ms();
        let client = Client {
            secret,
            address: address.clone(),
            connection_address: None,
            player_id,
            source_id,
            // `BaseServerAudioSource.state` starts at 1.
            source_state: 1,
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
                // Upstream mints `UUID.randomUUID()` for the player's source when the
                // source is first created; minting it at registration gives the same
                // guarantee (stable per player) with one less lazy branch.
                source_id: crate::state::VoiceServerState::generate_secret_uuid(
                    seed ^ 0x5350_4157_4e5f_5349,
                ),
                public_key: None,
                muted: false,
                voice_disabled: false,
                microphone_muted: false,
                activation_distances: Vec::new(),
                position: None,
                world: None,
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

    /// Every registered player id, sorted — the cheap enumeration the vanish
    /// sync needs before it asks the host who can see whom.
    #[must_use]
    pub fn registered_player_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self.registered.keys().copied().collect();
        ids.sort();
        ids
    }

    /// One registration, if the player is known.
    #[must_use]
    pub fn registered_player(&self, player_id: &Uuid) -> Option<&RegisteredPlayer> {
        self.registered.get(player_id)
    }

    /// Replaces the whole vanish table in one go.
    ///
    /// The tick pump rebuilds this from the host's `canSee` on a schedule, so an
    /// *incremental* API would only have to handle storms of diffs. A pair
    /// `(viewer, target)` means the host keeps `viewer` from seeing `target`.
    pub fn set_hidden_players(&mut self, hidden: HashSet<(Uuid, Uuid)>) {
        self.hidden_players = hidden;
    }

    /// Whether `speaker` and `listener` are silent to each other because either
    /// one is vanished from the other's world (bidirectional, per the chosen
    /// vanish semantics).
    #[must_use]
    fn voice_muted_by_vanish(&self, speaker: &Uuid, listener: &Uuid) -> bool {
        self.hidden_players.contains(&(*speaker, *listener))
            || self.hidden_players.contains(&(*listener, *speaker))
    }

    /// Players with a **live UDP connection**, which is upstream's `hasVoiceChat()`.
    ///
    /// Upstream's broadcasts only ever reach players whose connection was added — a
    /// player who joined Minecraft but never completed the UDP handshake gets nothing.
    #[must_use]
    pub fn connected_player_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self
            .by_secret
            .values()
            .filter_map(|client| client.player_id)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// The client record for a player, if it is connected.
    #[must_use]
    pub fn client_by_player(&self, player_id: &Uuid) -> Option<&Client> {
        self.by_secret
            .values()
            .find(|client| client.player_id.as_ref() == Some(player_id))
    }

    /// Applies a `PlayerStatePacket`, returning whether anything actually changed.
    ///
    /// Upstream only broadcasts when one of the two booleans flipped
    /// (`BaseVoicePlayer.setVoiceDisabled` / `setMicrophoneMuted` return the change), and
    /// its `hasVoiceChat()` guard means a player who is not connected is not tracked at
    /// all.
    pub fn set_player_state(
        &mut self,
        player_id: &Uuid,
        voice_disabled: bool,
        microphone_muted: bool,
    ) -> bool {
        let connected = self.client_by_player(player_id).is_some();
        let Some(player) = self.registered.get_mut(player_id) else {
            return false;
        };
        if !connected {
            return false;
        }
        let changed =
            player.voice_disabled != voice_disabled || player.microphone_muted != microphone_muted;
        player.voice_disabled = voice_disabled;
        player.microphone_muted = microphone_muted;
        changed
    }

    /// Applies a `PlayerActivationDistancesPacket`.
    ///
    /// Unknown activations are skipped, like upstream (`PlayerChannelHandler` looks each
    /// id up and silently ignores the ones it does not know).
    pub fn set_activation_distances(&mut self, player_id: &Uuid, distances: Vec<(Uuid, i32)>) {
        let known: Vec<Uuid> = self
            .config()
            .activations()
            .into_iter()
            .map(|activation| activation.id)
            .collect();
        let Some(player) = self.registered.get_mut(player_id) else {
            return;
        };
        player.activation_distances = distances
            .into_iter()
            .filter(|(id, _)| known.contains(id))
            .collect();
    }

    /// The distance the player last chose for `activation_id`, if any.
    ///
    /// `None` until the player's first `PlayerActivationDistancesPacket` — the
    /// same "never set" state upstream's `DistanceVisualizePacket` reply keys on.
    #[must_use]
    pub fn activation_distance(&self, player_id: &Uuid, activation_id: Uuid) -> Option<i32> {
        self.registered
            .get(player_id)?
            .activation_distances
            .iter()
            .find(|(id, _)| *id == activation_id)
            .map(|(_, distance)| *distance)
    }

    /// Records where a player is, which is what makes the proximity relay possible.
    pub fn set_position(
        &mut self,
        player_id: &Uuid,
        world: Option<String>,
        position: (f64, f64, f64),
    ) {
        if let Some(player) = self.registered.get_mut(player_id) {
            player.position = Some(position);
            if world.is_some() {
                player.world = world;
            }
        }
    }

    /// The `SourceInfo` a listener is told about `player_id`'s stream.
    ///
    /// Upstream answers a `SourceInfoRequestPacket` with
    /// `VoiceServerPlayerSource.createSourceInfo` — the `PLAYER` variant: the shared
    /// `SourceInfoBase` header plus the player's own `VoicePlayerInfo`. The base's
    /// `addon_id` is empty for the built-in sources, the decoder is the default
    /// `OpusDecoderInfo` (a bare `"opus"` codec with no parameters), and the source is
    /// published on the proximity line.
    #[must_use]
    pub fn player_source_info(&self, player_id: &Uuid) -> Option<SourceInfo> {
        let player = self.registered.get(player_id)?;
        let client = self.client_by_player(player_id)?;
        let nick = player.name.clone().unwrap_or_default();
        Some(SourceInfo::Player(PlayerSourceInfo {
            base: SourceInfoBase {
                addon_id: String::new(),
                id: player.source_id,
                name: player.name.clone(),
                state: client.source_state,
                decoder_info: Some(CodecInfo {
                    name: "opus".to_string(),
                    params: Vec::new(),
                }),
                stereo: false,
                line_id: self.config().source_lines()[0].id,
                // Upstream's player source exposes its icon while the stream is live;
                // a hidden icon is a per-player setting this server does not have.
                icon_visible: true,
                angle: 0,
            },
            player_info: VoicePlayerInfo {
                player_id: player.player_id,
                player_nick: nick,
                // `createPlayerInfo()` sets `muted` from the mute manager, so the
                // client's volume tab shows the server mute.
                muted: player.muted || crate::mute::is_muted(&player.player_id, now_ms()),
                voice_disabled: player.voice_disabled,
                microphone_muted: player.microphone_muted,
            },
        }))
    }

    /// The source id a listener sees audio from for `player_id`.
    #[must_use]
    pub fn source_id_of(&self, player_id: &Uuid) -> Option<Uuid> {
        self.registered
            .get(player_id)
            .map(|player| player.source_id)
    }

    /// The player publishing `source_id`, if any.
    #[must_use]
    pub fn player_for_source_id(&self, source_id: &Uuid) -> Option<Uuid> {
        self.registered
            .values()
            .find(|player| player.source_id == *source_id)
            .map(|player| player.player_id)
    }

    /// The `VoicePlayerInfo` this server advertises for a player.
    #[must_use]
    pub fn voice_player_info(&self, player_id: &Uuid) -> Option<VoicePlayerInfo> {
        let player = self.registered.get(player_id)?;
        Some(VoicePlayerInfo {
            player_id: player.player_id,
            player_nick: player.name.clone().unwrap_or_default(),
            // `createPlayerInfo()` sets `muted` from the mute manager, so the
            // client's volume tab shows the server mute.
            muted: player.muted || crate::mute::is_muted(player_id, now_ms()),
            voice_disabled: player.voice_disabled,
            microphone_muted: player.microphone_muted,
        })
    }

    /// The `PlayerInfoUpdatePacket` broadcast reflecting a player's current state.
    ///
    /// The `vmute`/`vunmute` commands push this after touching the mute store so
    /// every voice client's volume tab picks up the change, exactly like upstream's
    /// `broadcastPlayerInfoUpdate` after a mute change.
    #[must_use]
    pub fn player_info_update(&self, player_id: &Uuid) -> Option<Outbound> {
        let player_info = self.voice_player_info(player_id)?;
        let packet = TcpPacket::PlayerInfoUpdate(PlayerInfoUpdatePacket { player_info });
        Some(Outbound::broadcast(self.control().encode(&packet)))
    }

    /// The `SelfSourceInfo` a speaker is sent when its stream ends.
    ///
    /// Upstream fills `sequenceNumber = -1` (`SelfActivationHelper.kt:96`) and the
    /// activation the player was last using, so the client can close its own source.
    #[must_use]
    pub fn self_source_info(&self, player_id: &Uuid, activation_id: Uuid) -> SelfSourceInfo {
        SelfSourceInfo {
            source_info: self
                .player_source_info(player_id)
                .unwrap_or_else(|| self.source_info_placeholder(player_id)),
            player_id: *player_id,
            activation_id,
            sequence_number: -1,
        }
    }

    /// A `SourceInfo` for a player whose connection has just gone: the fields the client
    /// needs to recognise the source it must close.
    fn source_info_placeholder(&self, player_id: &Uuid) -> SourceInfo {
        let (source_id, nick) = self
            .registered
            .get(player_id)
            .map_or((Uuid::nil(), String::new()), |player| {
                (player.source_id, player.name.clone().unwrap_or_default())
            });
        SourceInfo::Player(PlayerSourceInfo {
            base: SourceInfoBase {
                addon_id: String::new(),
                id: source_id,
                name: None,
                state: 1,
                decoder_info: None,
                stereo: false,
                line_id: self.config().source_lines()[0].id,
                icon_visible: false,
                angle: 0,
            },
            player_info: VoicePlayerInfo {
                player_id: *player_id,
                player_nick: nick,
                muted: false,
                voice_disabled: false,
                microphone_muted: false,
            },
        })
    }

    /// `VoiceServerProximitySource.getListeners`: who hears `speaker`'s audio.
    ///
    /// Returns player ids, because the caller may need either their connection (to send a
    /// datagram) or their control-plane identity (to send an audio-end). The activation
    /// must be one this server offers, and its distance is clamped exactly like
    /// `Activation.calculateAllowedDistance` before the radius is computed.
    #[must_use]
    pub fn proximity_listener_ids(
        &self,
        speaker: &Uuid,
        activation_id: Uuid,
        requested_distance: i32,
    ) -> Vec<Uuid> {
        let Some(activation) = self
            .config()
            .activations()
            .into_iter()
            .find(|activation| activation.id == activation_id)
        else {
            return Vec::new();
        };
        let distance = calculate_allowed_distance(&activation, requested_distance);
        // The configured extra broadcast distance, `voice.maxExtraAudioBroadcastDistance`
        // (`VoiceServerConfig.java:137`).
        let radius = distance
            .saturating_add(self.config().max_extra_audio_broadcast_distance())
            .min(distance.saturating_mul(2));
        let Some(source) = self.registered.get(speaker) else {
            return Vec::new();
        };
        // `matchFilters` also drops a speaker whose own voice chat is off; upstream never
        // gets that far because a disabled client stops sending audio.
        if source.voice_disabled {
            return Vec::new();
        }

        let mut listeners: Vec<Uuid> = self
            .registered
            .values()
            .filter(|candidate| candidate.player_id != *speaker)
            .filter(|candidate| !candidate.voice_disabled)
            .filter(|candidate| !self.voice_muted_by_vanish(speaker, &candidate.player_id))
            .filter(|candidate| within_proximity(source, candidate, radius))
            .map(|candidate| candidate.player_id)
            .collect();
        listeners.sort();
        listeners
    }

    /// Pings idle connections and retires the silent ones.
    ///
    /// Mirrors `NettyUdpKeepAlive.tick`: a connection that has sent nothing for
    /// `timeout_ms` is removed, and every other connection gets an empty
    /// `PingPacket`. Upstream spreads the pings with 1.5-3s of jitter; deriving
    /// that jitter from the secret keeps the spread without needing an RNG in the
    /// guest. Retiring a connection also broadcasts a `PlayerDisconnectPacket`, and
    /// leaves the *registration* alone — upstream's secret is sticky, so the same
    /// client may come back on the same secret.
    pub fn keep_alive(&mut self, now: u64, timeout_ms: u64) -> Sweep {
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

        let mut control = Vec::new();
        let mut timed_out_players = Vec::new();
        for secret in expired {
            if let Some(player_id) = self.player_for_secret(&secret) {
                control.extend(self.control.disconnected(player_id));
                timed_out_players.push(player_id);
                tracing::info!(
                    %player_id,
                    %secret,
                    timeout_ms,
                    "a voice connection timed out after {timeout_ms} ms of silence; the \
                     registration survives, so the client will be re-asked for its info"
                );
            }
            self.remove_connection(&secret);
        }

        let outgoing = due
            .into_iter()
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
            .collect();

        Sweep {
            outgoing,
            control,
            timed_out_players,
        }
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
    ///
    /// A datagram whose secret is registered but not yet connected does **only one
    /// thing**: it creates the connection (upstream's `handlePacket` lives in the branch
    /// that datagram does not take) and produces the registration burst — config, player
    /// list, player-info broadcast — as control-plane messages. The first datagram of a
    /// session is therefore never relayed, exactly like upstream.
    pub fn handle_datagram(&mut self, from: &str, data: &[u8]) -> Handled {
        let envelope = match self.codec.decode_header(data) {
            Ok(Some(envelope)) => envelope,
            // Wrong magic (`Ok(None)`) or a truncated header (`Err`).
            Ok(None) | Err(_) => {
                self.dropped = self.dropped.saturating_add(1);
                self.malformed_drops = self.malformed_drops.saturating_add(1);
                // A wrong magic means something reached this port that is not Plasmo
                // Voice at all: a stray service, a scanner, or a client talking to the
                // wrong process. Worth naming once per log level, not once per packet.
                if self.malformed_drops <= UNKNOWN_SECRET_WARNINGS {
                    tracing::warn!(
                        from,
                        bytes = data.len(),
                        total = self.malformed_drops,
                        "dropped a {}-byte datagram from {from}: not a Plasmo Voice packet \
                         ({} so far)",
                        data.len(),
                        self.malformed_drops
                    );
                } else {
                    tracing::debug!(from, bytes = data.len(), "dropped a malformed datagram");
                }
                return Handled::nothing();
            }
        };
        let secret = envelope.secret;

        let mut registered_now = None;
        if self.by_secret.contains_key(&secret) {
            // A client that roamed to another address is followed, like
            // upstream's `setRemoteAddress`.
            self.readdress(secret, from);
        } else if let Some(player_id) = self.player_by_secret.get(&secret).copied() {
            // First datagram from a *registered* player: this is where the UDP
            // connection is born, and where upstream then sends the burst.
            self.add_connection(secret, from.to_string(), Some(player_id));
            registered_now = Some(player_id);
            // The moment the whole handshake exists for. If this line is missing from a
            // log while a client reports "can't connect to the UDP server", the client's
            // datagrams are not arriving at all — the fault is upstream of this plugin
            // (address, port, firewall), not in the protocol. If it *is* present and the
            // client still times out, the fault is on the way out: see the send-failure
            // warning in `runtime::send_datagrams`.
            tracing::info!(
                %player_id,
                %secret,
                from,
                "a voice client opened its UDP connection from {from} (player {player_id})"
            );
        } else {
            self.dropped = self.dropped.saturating_add(1);
            self.unknown_secret_drops = self.unknown_secret_drops.saturating_add(1);
            // Datagrams whose secret this server never minted are the hardest failure to
            // diagnose from outside: they *prove* the client reached the socket, so every
            // "the port is closed" theory is already wrong, and yet nothing is answered.
            // Say so for the first few, then drop to debug — a public port gets scanned.
            if self.unknown_secret_drops <= UNKNOWN_SECRET_WARNINGS {
                tracing::warn!(
                    from,
                    %secret,
                    total = self.unknown_secret_drops,
                    "dropped a voice datagram from {from}: secret {secret} was never issued \
                     by this server ({} so far)",
                    self.unknown_secret_drops
                );
            } else {
                tracing::debug!(from, %secret, "dropped a voice datagram with an unknown secret");
            }
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

        let was_ping = matches!(packet, UdpPacket::Ping(_));

        if let Some(player_id) = registered_now {
            // A ping carries the endpoint the *client* believes it is talking to, and
            // upstream records it in `connectionAddress` (`NettyPacketHandler:60-65`) —
            // a purely informational field. It is emphatically **not** where datagrams
            // are sent: those go to `remoteAddress`, the datagram's sender (`:59`).
            // Conflating the two would make the server send audio to itself.
            if let UdpPacket::Ping(ping) = &packet
                && let (Some(ip), Some(port)) = (ping.server_ip.as_deref(), ping.server_port)
                && let Some(client) = self.by_secret.get_mut(&secret)
            {
                client.connection_address = Some(format!("{ip}:{port}"));
            }
            return Handled {
                outgoing: Vec::new(),
                control: self.control.registration_burst(self, player_id),
                was_ping,
            };
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
                control: Vec::new(),
                was_ping: true,
            },

            // Player audio: never forwarded as-is. Upstream builds a clientbound
            // `SourceAudioPacket` for every listener in range and a `SelfAudioInfoPacket`
            // for the speaker itself.
            UdpPacket::PlayerAudio(audio) => {
                let (outgoing, control) = self.relay_audio(secret, &audio);
                Handled {
                    outgoing,
                    control,
                    was_ping: false,
                }
            }

            // Source / activation audio from the server itself never arrives
            // inbound; treat anything else as unknown traffic.
            _ => {
                self.dropped = self.dropped.saturating_add(1);
                Handled::nothing()
            }
        }
    }

    /// The upstream proximity fan-out for one `PlayerAudioPacket`.
    ///
    /// The chain this reproduces is
    /// `NettyUdpServerConnection.handle` → `VoiceServerActivationManager.onPlayerSpeak`
    /// → `ProximityServerActivationHelper.onActivation` →
    /// `VoiceServerProximitySource.sendAudioPacket`:
    ///
    /// 1. the activation id must be one this server offers, and the speaker must not be
    ///    muted (server mute or the client's own microphone mute);
    /// 2. the distance is **clamped** to one the activation allows;
    /// 3. every *other* connected player within `min(distance + maxExtra, distance * 2)`
    ///    gets a fresh `SourceAudioPacket` — same payload bytes, but its own secret and a
    ///    fresh timestamp, because those live in the part of the frame that is per-client;
    /// 4. the speaker gets a `SelfAudioInfoPacket` instead, so its own overlay can show
    ///    the live stream without the server looping audio back to it.
    ///
    /// The speaker's audio is never encoded once and copied: the seconds-long payload is
    /// shared, the ~20-byte envelope is not.
    fn relay_audio(
        &self,
        speaker_secret: Uuid,
        audio: &plasmo_voice_core::wire::PlayerAudioPacket,
    ) -> (Vec<Outgoing>, Vec<Outbound>) {
        let refused = (Vec::new(), Vec::new());
        let Some(speaker) = self.by_secret.get(&speaker_secret) else {
            return refused;
        };
        let Some(player_id) = speaker.player_id else {
            return refused;
        };
        let Some(player) = self.registered.get(&player_id) else {
            return refused;
        };

        // `getActivationById(activationId)`; an activation this server does not offer
        // means the packet is not ours to relay.
        let Some(activation) = self
            .config()
            .activations()
            .into_iter()
            .find(|activation| activation.id == audio.activation_id)
        else {
            return refused;
        };
        // `isMicrophoneMuted()` / server-mute guards, checked before anything is built.
        // The server mute lives in [`crate::mute`]: a player muted with `vmute` has its
        // audio refused here regardless of what its own client sends in `PlayerStatePacket`.
        if player.microphone_muted
            || player.muted
            || player.voice_disabled
            || crate::mute::is_muted(&player_id, now_ms())
        {
            return refused;
        }

        let distance = calculate_allowed_distance(&activation, i32::from(audio.distance));
        let distance_short = i16::try_from(distance).unwrap_or(i16::MAX);
        let timestamp = i64::try_from(now_ms()).unwrap_or(i64::MAX);

        let mut outgoing = Vec::new();
        for listener_id in
            self.proximity_listener_ids(&player_id, audio.activation_id, i32::from(audio.distance))
        {
            let Some(listener) = self.client_by_player(&listener_id) else {
                continue;
            };
            let packet = UdpPacket::SourceAudio(SourceAudioPacket {
                sequence_number: audio.sequence_number,
                data: audio.data.clone(),
                source_id: speaker.source_id,
                source_state: speaker.source_state,
                distance: distance_short,
            });
            if let Ok(data) = self.codec.encode(&packet, listener.secret, timestamp) {
                outgoing.push(Outgoing {
                    to: listener.address.clone(),
                    data,
                });
            }
        }

        // `SelfActivationHelper.sendAudioInfo`: the payload is only echoed back when its
        // size changed, which on this path never happens.
        let self_info = UdpPacket::SelfAudioInfo(SelfAudioInfoPacket {
            source_id: speaker.source_id,
            sequence_number: audio.sequence_number,
            data: None,
            distance: distance_short,
        });
        if let Ok(data) = self.codec.encode(&self_info, speaker.secret, timestamp) {
            outgoing.push(Outgoing {
                to: speaker.address.clone(),
                data,
            });
        }

        (outgoing, Vec::new())
    }
}

/// `VoiceServerProximitySource.getListeners`' spatial test.
///
/// Upstream reads both positions live from the Minecraft server and requires the same
/// world and a squared 3-D distance within the radius. This server only *knows* what the
/// host told it, so an unknown position or world means "cannot verify" and the listener is
/// skipped: refusing to relay is recoverable, relaying audio across a world boundary is
/// not.
#[must_use]
fn within_proximity(speaker: &RegisteredPlayer, listener: &RegisteredPlayer, radius: i32) -> bool {
    if speaker.world.is_none() || listener.world.is_none() || speaker.world != listener.world {
        return false;
    }
    let (Some(from), Some(to)) = (speaker.position, listener.position) else {
        return false;
    };
    let dx = from.0 - to.0;
    let dy = from.1 - to.1;
    let dz = from.2 - to.2;
    let squared = dx.mul_add(dx, dy.mul_add(dy, dz * dz));
    let radius = f64::from(radius);
    squared <= radius * radius
}

#[cfg(test)]
mod tests {
    use super::*;
    use plasmo_voice_core::wire::{PingPacket, PlayerAudioPacket};

    const SERVER_SECRET: Uuid = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
    const CLIENT_SECRET: Uuid = Uuid::from_u128(0xaaaa_bbbb_cccc_dddd_eeee_ffff_0000_1111);

    /// A fixed server-wide AES key, so tests stay deterministic and the data
    /// plane does not touch the guest's random source.
    fn test_aes_key() -> crate::crypto::AesKey {
        crate::crypto::AesKey::from_hex("00112233445566778899aabbccddeeff")
            .expect("the test key is 32 lowercase hex chars")
    }

    /// The server under test, on a fixed secret.
    fn server() -> VoiceServer {
        VoiceServer::new(SERVER_SECRET, test_aes_key())
    }

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
        let mut server = server();
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
        assert_eq!(client.player_id, Some(player_id));
        assert_eq!(
            server.client_by_address("10.0.0.1:5000").map(|c| c.secret),
            Some(secret)
        );
        // The connection's source id is the player's, minted at registration, so a
        // listener told about this source before a reconnect still recognises it.
        assert_eq!(client.source_id, server.source_id_of(&player_id).unwrap());
        assert_eq!(client.source_state, 1, "state starts at 1");
    }

    #[test]
    fn the_first_datagram_of_a_session_only_registers_the_connection() {
        let mut server = server();
        let secret = register(&mut server, 0xa11ce);

        // Upstream's `channelRead0` creates the connection and sends the burst; the
        // packet body is handled by `handlePacket`, which that branch never reaches.
        // So even *audio* in a session's first datagram is never relayed.
        let activation = server.config().activations()[0].id;
        let audio = PlayerAudioPacket {
            sequence_number: 1,
            data: vec![1, 2, 3],
            activation_id: activation,
            distance: 16,
            stereo: false,
        };
        let wire = codec()
            .encode(&UdpPacket::PlayerAudio(audio), secret, 0)
            .expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);

        assert!(handled.outgoing.is_empty(), "nothing is relayed or echoed");
        assert_eq!(server.connection_count(), 1, "but the connection exists");
        assert_eq!(handled.control.len(), 3, "config, player list and update");
        assert_eq!(server.dropped(), 0, "this was not dropped, it was handled");

        // The next datagram with the same secret *is* handled normally.
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(
            handled.control.is_empty(),
            "the burst happens once per connection"
        );
    }

    #[test]
    fn the_registration_burst_is_config_then_player_list_then_update() {
        use plasmo_voice_core::TcpPacket;

        let mut server = server();
        let secret = register(&mut server, 0xa11ce);
        let wire = codec().encode(&empty_ping(), secret, 1).expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);

        let ids: Vec<u8> = handled
            .control
            .iter()
            .map(|message| *message.payload.first().expect("every payload has an id"))
            .collect();
        // ConfigPacket 3, PlayerListPacket 7, PlayerInfoUpdatePacket 8.
        assert_eq!(ids, vec![3, 7, 8]);

        // The first two are unicast to the joiner; the update is a broadcast, because
        // upstream flips `connected` before sending it and everyone hears about it.
        assert_eq!(handled.control[0].to, Some(Uuid::from_u128(0xa11ce)));
        assert_eq!(handled.control[1].to, Some(Uuid::from_u128(0xa11ce)));
        assert_eq!(handled.control[2].to, None);

        let codec = plasmo_voice_core::TcpCodec::new();
        let decoded = codec
            .decode(&handled.control[0].payload, PacketDirection::Client)
            .expect("decode ok")
            .expect("packet");
        match decoded {
            TcpPacket::Config(config) => {
                assert_eq!(config.server_id, SERVER_SECRET);
                assert_eq!(config.activations.len(), 1);
                assert_eq!(config.activations[0].name, "proximity");
                assert_eq!(config.source_lines[0].name, "proximity");
                assert!(config.player_icon_config.is_some());
                assert!(config.encryption.is_none());
            }
            other => panic!("expected config, got {other:?}"),
        }

        // The player list contains the joiner itself: its connection already exists.
        let decoded = codec
            .decode(&handled.control[1].payload, PacketDirection::Client)
            .expect("decode ok")
            .expect("packet");
        match decoded {
            TcpPacket::PlayerList(list) => {
                assert_eq!(list.players.len(), 1);
                assert_eq!(list.players[0].player_id, Uuid::from_u128(0xa11ce));
                assert_eq!(list.players[0].player_nick, "player-a11ce");
            }
            other => panic!("expected player list, got {other:?}"),
        }
    }

    #[test]
    fn a_client_that_roams_is_followed_to_its_new_address() {
        let mut server = server();
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
        let mut server = server();
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
        let mut server = server();
        let bob_secret = register(&mut server, 0xb0b);
        server.add_connection(
            bob_secret,
            "2.2.2.2:20".to_string(),
            Some(Uuid::from_u128(0xb0b)),
        );

        let activation = server.config().activations()[0].id;
        let audio = PlayerAudioPacket {
            sequence_number: 1,
            data: vec![1],
            activation_id: activation,
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
        assert!(handled.control.is_empty());
        assert_eq!(server.dropped(), 1);
    }

    #[test]
    fn unregistering_a_player_drops_its_connection_and_its_secret() {
        let mut server = server();
        let player = Uuid::from_u128(0xa11ce);
        let secret = server.register_player(player, Some("alice".into()));
        server.add_connection(secret, "1.1.1.1:10".to_string(), Some(player));

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
        let mut server = server();
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
        let mut server = server();
        let alice = register(&mut server, 0xa11ce);
        let bob = register(&mut server, 0xb0b);

        assert_ne!(alice, bob);
        assert_eq!(server.registered_player_count(), 2);
        assert_eq!(server.registered_players().len(), 2);
    }

    #[test]
    fn keep_alive_pings_idle_connections_and_retires_silent_ones() {
        let mut server = server();
        let secret = register(&mut server, 0xc0ffee);
        server.add_connection(secret, "1.1.1.1:10".to_string(), None);
        let now = now_ms();

        // Upstream's `sentKeepAlive` starts unset, so the first sweep pings at
        // once — and that first ping is what makes a real client "connected".
        let first = server.keep_alive(now, KEEP_ALIVE_TIMEOUT_MS);
        assert_eq!(first.outgoing.len(), 1);
        assert_eq!(first.outgoing[0].to, "1.1.1.1:10");
        assert!(first.control.is_empty());
        match codec()
            .decode(&first.outgoing[0].data, PacketDirection::Server)
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
                .decode_header(&first.outgoing[0].data)
                .expect("header")
                .expect("envelope")
                .secret,
            secret
        );

        // Not again inside the jittered interval...
        let second = server.keep_alive(now + 10, KEEP_ALIVE_TIMEOUT_MS);
        assert!(second.outgoing.is_empty() && second.control.is_empty());

        // ...but a connection silent past the timeout is retired, and its departure is
        // broadcast over the control plane while the registration survives.
        let expired = server.keep_alive(now + KEEP_ALIVE_TIMEOUT_MS + 1, KEEP_ALIVE_TIMEOUT_MS);
        assert!(expired.outgoing.is_empty());
        assert_eq!(expired.control.len(), 1, "a PlayerDisconnect broadcast");
        assert_eq!(expired.control[0].to, None);
        assert_eq!(expired.control[0].payload[0], 9, "PlayerDisconnectPacket");
        assert_eq!(server.connection_count(), 0);
        assert_eq!(
            server.registered_player_count(),
            1,
            "the secret is sticky: only the connection died"
        );
    }

    #[test]
    fn keep_alive_does_not_expire_a_client_that_keeps_talking() {
        let mut server = server();
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
        let mut server = server();
        let handled = server.handle_datagram("10.0.0.1:5000", b"hello there");
        assert!(handled.outgoing.is_empty());
        assert!(!handled.was_ping);
        assert_eq!(server.dropped(), 1);
    }

    #[test]
    fn a_timed_out_connection_reports_the_player_for_a_new_handshake() {
        let mut server = server();
        let player = Uuid::from_u128(0xc0ffee);
        let secret = server.register_player(player, Some("carol".into()));
        server.add_connection(secret, "1.1.1.1:10".to_string(), Some(player));

        let now = now_ms();
        let _ = server.keep_alive(now, KEEP_ALIVE_TIMEOUT_MS);
        let expired = server.keep_alive(now + KEEP_ALIVE_TIMEOUT_MS + 1, KEEP_ALIVE_TIMEOUT_MS);

        // Upstream re-asks a timed-out player for its info
        // (`NettyUdpKeepAlive:48`), so the runtime needs the player id, not just the
        // disconnect broadcast.
        assert_eq!(expired.timed_out_players, vec![player]);
        assert_eq!(server.connection_count(), 0);
        assert_eq!(
            server.registered_player_count(),
            1,
            "only the connection died; the registration is what makes the retry possible"
        );

        // A sweep with nothing to retire reports nobody.
        let quiet = server.keep_alive(now + KEEP_ALIVE_TIMEOUT_MS + 2, KEEP_ALIVE_TIMEOUT_MS);
        assert!(quiet.timed_out_players.is_empty());
    }

    #[test]
    fn truncated_ping_is_dropped() {
        let mut server = server();
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
        let mut server = server();
        let alice = Uuid::from_u128(0xa11ce);
        server.add_connection(CLIENT_SECRET, "1.2.3.4:10".to_string(), Some(alice));
        assert_eq!(server.connection_count(), 1);
        assert_eq!(
            server.client_by_secret(&CLIENT_SECRET).map(|c| c.player_id),
            Some(Some(alice))
        );

        // Re-adding the same secret from a new address moves it.
        server.add_connection(CLIENT_SECRET, "1.2.3.4:11".to_string(), Some(alice));
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

    /// Registers a player, opens its UDP connection with a ping, and places it.
    ///
    /// Returns the secret. The ping is what a real client sends first, so the tests walk
    /// the same path the running server does.
    fn connect(
        server: &mut VoiceServer,
        player: u128,
        address: &str,
        world: &str,
        position: (f64, f64, f64),
    ) -> Uuid {
        let secret = register(server, player);
        let ping = codec().encode(&empty_ping(), secret, 1).expect("encode");
        server.handle_datagram(address, &ping);
        server.set_position(&Uuid::from_u128(player), Some(world.to_string()), position);
        secret
    }

    /// Builds a `PlayerAudioPacket` for the server's own proximity activation.
    fn voice_packet(server: &VoiceServer, sequence_number: i64, data: Vec<u8>) -> UdpPacket {
        UdpPacket::PlayerAudio(PlayerAudioPacket {
            sequence_number,
            data,
            activation_id: server.config().activations()[0].id,
            distance: 16,
            stereo: false,
        })
    }

    #[test]
    fn player_audio_becomes_source_audio_for_listeners_in_range() {
        let mut server = server();
        let alice = Uuid::from_u128(0xa11ce);
        let alice_secret = connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        let bob_secret = connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (10.0, 0.0, 0.0));

        let wire = codec()
            .encode(&voice_packet(&server, 7, vec![1, 2, 3, 4]), alice_secret, 0)
            .expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);

        assert_eq!(
            handled.outgoing.len(),
            2,
            "one listener plus the speaker's own info"
        );
        let to_bob = handled
            .outgoing
            .iter()
            .find(|datagram| datagram.to == "2.2.2.2:20")
            .expect("bob hears alice");
        let to_alice = handled
            .outgoing
            .iter()
            .find(|datagram| datagram.to == "1.1.1.1:10")
            .expect("alice is told about her own stream");

        // The listener's frame is addressed to *its* secret: alice's must not match.
        assert_eq!(
            codec()
                .decode_header(&to_bob.data)
                .expect("header")
                .expect("envelope")
                .secret,
            bob_secret
        );
        // `SourceAudioPacket` is a CLIENT-direction id (upstream 0x03); a client that
        // receives it decodes it as such.
        match codec()
            .decode(&to_bob.data, PacketDirection::Client)
            .expect("decode")
            .expect("packet")
        {
            UdpPacket::SourceAudio(source) => {
                assert_eq!(source.sequence_number, 7, "the sequence is passed through");
                assert_eq!(
                    source.data,
                    vec![1, 2, 3, 4],
                    "the payload is not re-encoded"
                );
                assert_eq!(
                    source.source_id,
                    server.source_id_of(&alice).expect("alice has a source"),
                    "the listener keys audio by the speaker's source id, not a secret"
                );
                assert_eq!(source.source_state, 1, "the source state travels with it");
                assert_eq!(source.distance, 16, "a distance the activation allows");
            }
            other => panic!("expected source audio, got {other:?}"),
        }

        // The speaker gets `SelfAudioInfoPacket` — no payload echo, because the size did
        // not change.
        match codec()
            .decode(&to_alice.data, PacketDirection::Client)
            .expect("decode")
            .expect("packet")
        {
            UdpPacket::SelfAudioInfo(info) => {
                assert_eq!(info.source_id, server.source_id_of(&alice).unwrap());
                assert_eq!(info.sequence_number, 7);
                assert!(info.data.is_none(), "the payload is never echoed back");
                assert_eq!(info.distance, 16);
            }
            other => panic!("expected self audio info, got {other:?}"),
        }
    }

    #[test]
    fn audio_is_only_relayed_inside_the_activation_radius() {
        let mut server = server();
        let alice_secret = connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        // 16 blocks is the default distance, so upstream's radius is
        // `min(16 + 16, 16 * 2) == 32`.
        connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (31.0, 0.0, 0.0));
        connect(&mut server, 0xc0c, "3.3.3.3:30", "world", (40.0, 0.0, 0.0));

        let wire = codec()
            .encode(&voice_packet(&server, 1, vec![0]), alice_secret, 0)
            .expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);

        let recipients: Vec<&str> = handled
            .outgoing
            .iter()
            .map(|datagram| datagram.to.as_str())
            .collect();
        assert!(recipients.contains(&"2.2.2.2:20"), "31 blocks is inside 32");
        assert!(
            !recipients.contains(&"3.3.3.3:30"),
            "40 blocks is outside the radius"
        );
    }

    #[test]
    fn audio_never_crosses_a_world_boundary() {
        let mut server = server();
        let alice_secret = connect(
            &mut server,
            0xa11ce,
            "1.1.1.1:10",
            "overworld",
            (0.0, 0.0, 0.0),
        );
        connect(&mut server, 0xb0b, "2.2.2.2:20", "nether", (0.0, 0.0, 0.0));

        let wire = codec()
            .encode(&voice_packet(&server, 1, vec![0]), alice_secret, 0)
            .expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(
            handled
                .outgoing
                .iter()
                .all(|datagram| datagram.to != "2.2.2.2:20"),
            "the same coordinates in another world are not in range"
        );
    }

    #[test]
    fn a_player_without_a_known_position_is_never_a_listener() {
        let mut server = server();
        let alice_secret = connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        // Bob is connected but the host has not told us where he is.
        register(&mut server, 0xb0b);
        let ping = codec()
            .encode(
                &empty_ping(),
                server.secret_for_player(&Uuid::from_u128(0xb0b)).unwrap(),
                1,
            )
            .expect("encode");
        server.handle_datagram("2.2.2.2:20", &ping);

        let wire = codec()
            .encode(&voice_packet(&server, 1, vec![0]), alice_secret, 0)
            .expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(
            handled
                .outgoing
                .iter()
                .all(|datagram| datagram.to != "2.2.2.2:20"),
            "an unknown position means 'cannot verify', so no relay"
        );
    }

    #[test]
    fn an_activation_this_server_does_not_offer_is_never_relayed() {
        let mut server = server();
        let alice_secret = connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (1.0, 0.0, 0.0));

        let packet = UdpPacket::PlayerAudio(PlayerAudioPacket {
            sequence_number: 1,
            data: vec![0],
            activation_id: Uuid::from_u128(0xbeef),
            distance: 16,
            stereo: false,
        });
        let wire = codec().encode(&packet, alice_secret, 0).expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(handled.outgoing.is_empty(), "unknown activation, no relay");
    }

    #[test]
    fn a_muted_or_disabled_speaker_is_never_relayed() {
        for (voice_disabled, microphone_muted) in [(true, false), (false, true)] {
            let mut server = server();
            let alice_secret =
                connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
            connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (1.0, 0.0, 0.0));
            assert!(server.set_player_state(
                &Uuid::from_u128(0xa11ce),
                voice_disabled,
                microphone_muted
            ));

            let wire = codec()
                .encode(&voice_packet(&server, 1, vec![0]), alice_secret, 0)
                .expect("encode");
            let handled = server.handle_datagram("1.1.1.1:10", &wire);
            assert!(
                handled.outgoing.is_empty(),
                "a mute the client reported must be honoured by the server too"
            );
        }
    }

    #[test]
    fn a_listener_with_voice_chat_off_hears_nothing() {
        let mut server = server();
        let alice_secret = connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (1.0, 0.0, 0.0));
        assert!(server.set_player_state(&Uuid::from_u128(0xb0b), true, false));

        let wire = codec()
            .encode(&voice_packet(&server, 1, vec![0]), alice_secret, 0)
            .expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        assert!(
            handled
                .outgoing
                .iter()
                .all(|datagram| datagram.to != "2.2.2.2:20"),
            "matchFilters drops a listener whose voice chat is off"
        );
    }

    #[test]
    fn the_relayed_distance_is_clamped_to_one_the_activation_allows() {
        let mut server = server();
        let alice_secret = connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (1.0, 0.0, 0.0));
        let activation = server.config().activations()[0].id;

        // 20 is not one of [8, 16, 32], so `calculateAllowedDistance` collapses it to
        // the activation's default.
        let packet = UdpPacket::PlayerAudio(PlayerAudioPacket {
            sequence_number: 1,
            data: vec![0],
            activation_id: activation,
            distance: 20,
            stereo: false,
        });
        let wire = codec().encode(&packet, alice_secret, 0).expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        let to_bob = handled
            .outgoing
            .iter()
            .find(|datagram| datagram.to == "2.2.2.2:20")
            .expect("bob hears alice");
        match codec()
            .decode(&to_bob.data, PacketDirection::Client)
            .expect("decode")
            .expect("packet")
        {
            UdpPacket::SourceAudio(source) => assert_eq!(source.distance, 16),
            other => panic!("expected source audio, got {other:?}"),
        }

        // A distance the activation *does* allow is passed through unchanged.
        let packet = UdpPacket::PlayerAudio(PlayerAudioPacket {
            sequence_number: 2,
            data: vec![0],
            activation_id: activation,
            distance: 32,
            stereo: false,
        });
        let wire = codec().encode(&packet, alice_secret, 0).expect("encode");
        let handled = server.handle_datagram("1.1.1.1:10", &wire);
        let to_bob = handled
            .outgoing
            .iter()
            .find(|datagram| datagram.to == "2.2.2.2:20")
            .expect("bob hears alice");
        match codec()
            .decode(&to_bob.data, PacketDirection::Client)
            .expect("decode")
            .expect("packet")
        {
            UdpPacket::SourceAudio(source) => assert_eq!(source.distance, 32),
            other => panic!("expected source audio, got {other:?}"),
        }
    }

    #[test]
    fn a_listener_can_look_up_the_source_info_of_whoever_it_hears() {
        let mut server = server();
        connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (1.0, 0.0, 0.0));
        let alice = Uuid::from_u128(0xa11ce);

        let source_id = server.source_id_of(&alice).expect("a source per player");
        assert_eq!(server.player_for_source_id(&source_id), Some(alice));
        assert_eq!(server.player_for_source_id(&Uuid::from_u128(7)), None);

        // A client that hears an unknown source asks for it; the answer must let it
        // play the audio, which means the right line, the right state and a decoder.
        let info = server
            .player_source_info(&alice)
            .expect("alice is connected");
        match info {
            SourceInfo::Player(source) => {
                assert_eq!(source.base.id, source_id);
                assert_eq!(source.base.name.as_deref(), Some("player-a11ce"));
                assert_eq!(source.base.state, 1);
                assert_eq!(source.base.line_id, server.config().source_lines()[0].id);
                assert_eq!(
                    source.base.decoder_info.map(|codec| codec.name),
                    Some("opus".to_string())
                );
                assert_eq!(source.player_info.player_id, alice);
                assert_eq!(source.player_info.player_nick, "player-a11ce");
            }
            other => panic!("expected a player source, got {other:?}"),
        }
    }

    #[test]
    fn source_direction_ids_are_not_accepted_inbound() {
        // SourceAudio is CLIENT-direction, i.e. the server *sends* it; a client
        // sending one must be ignored.
        let mut server = server();
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

    #[test]
    fn vanish_mutes_a_pair_in_both_directions() {
        let mut server = server();
        let alice_id = Uuid::from_u128(0xa11ce);
        let bob_id = Uuid::from_u128(0xb0b);
        connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (1.0, 0.0, 0.0));
        let activation = server.config().activations()[0].id;

        // Baseline: each hears the other.
        assert_eq!(
            server.proximity_listener_ids(&alice_id, activation, 16),
            vec![bob_id]
        );
        assert_eq!(
            server.proximity_listener_ids(&bob_id, activation, 16),
            vec![alice_id]
        );

        // Alice vanishes from Bob's world (`hide_player(alice, bob)` upstream):
        // the pair goes silent in both directions, the chosen vanish semantics.
        server.set_hidden_players(HashSet::from([(alice_id, bob_id)]));
        assert!(
            server
                .proximity_listener_ids(&alice_id, activation, 16)
                .is_empty()
        );
        assert!(
            server
                .proximity_listener_ids(&bob_id, activation, 16)
                .is_empty()
        );

        // The mirror direction behaves identically.
        server.set_hidden_players(HashSet::from([(bob_id, alice_id)]));
        assert!(
            server
                .proximity_listener_ids(&alice_id, activation, 16)
                .is_empty()
        );
        assert!(
            server
                .proximity_listener_ids(&bob_id, activation, 16)
                .is_empty()
        );
    }

    #[test]
    fn vanish_only_mutes_the_vanished_pair() {
        let mut server = server();
        let alice_id = Uuid::from_u128(0xa11ce);
        let bob_id = Uuid::from_u128(0xb0b);
        let carol_id = Uuid::from_u128(0xca20);
        connect(&mut server, 0xa11ce, "1.1.1.1:10", "world", (0.0, 0.0, 0.0));
        connect(&mut server, 0xb0b, "2.2.2.2:20", "world", (1.0, 0.0, 0.0));
        connect(&mut server, 0xca20, "3.3.3.3:30", "world", (0.0, 0.0, 1.0));
        let activation = server.config().activations()[0].id;

        server.set_hidden_players(HashSet::from([(alice_id, bob_id)]));

        // Carol hears both Alice and Bob; Alice and Bob both hear Carol.
        let alice_hear = server.proximity_listener_ids(&alice_id, activation, 16);
        let bob_hear = server.proximity_listener_ids(&bob_id, activation, 16);
        assert!(alice_hear.contains(&carol_id) && !alice_hear.contains(&bob_id));
        assert!(bob_hear.contains(&carol_id) && !bob_hear.contains(&alice_id));
        assert_eq!(
            server.proximity_listener_ids(&carol_id, activation, 16),
            vec![bob_id, alice_id]
        );
    }
}
