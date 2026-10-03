//! The Plasmo Voice control plane — the protocol that runs over `plasmo:voice/v2`.
//!
//! Upstream has **no TCP listener**: `ServerChannelHandler` hands every `plasmo:voice/v2`
//! plugin-message payload to `PacketTcpCodec` in the SERVER direction and
//! `VoiceTcpServerConnectionManager` answers on the same channel. This module is the Rust
//! equivalent of that manager plus its packet handlers, with no knowledge of Pumpkin: it
//! takes bytes and a player id, and returns the messages that must be delivered
//! ([`Outbound`]). The ABI shim in `channel.rs` does the delivering.
//!
//! The channel name lives here, next to the protocol it carries, because this module is
//! host-testable and `channel.rs` is not: a wrong channel name is a total, silent failure
//! (the client drops every payload and reports "Plasmo Voice is not installed on this
//! server"), so it needs a pinned assertion rather than a comment.
//!
//! ## The three phases of a connection
//!
//! 1. **Join** — the server asks the client who it is: `PlayerInfoRequestPacket` (id 2).
//!    Upstream retries this at +1/3/5/10/15 s until an answer arrives.
//! 2. **Answer** — the client sends `PlayerInfoPacket` (id 10) with its mod version and an
//!    RSA public key. The server gates on the *major* version, then replies
//!    `ConnectionPacket` (id 1) carrying the secret, host and port of the UDP server.
//! 3. **Registration** — the client's first datagram with that secret creates the UDP
//!    connection, and the server answers with the burst: `ConfigPacket` (id 3) →
//!    `PlayerListPacket` (id 7) → `PlayerInfoUpdatePacket` (id 8, broadcast).
//!
//! The order matters: upstream flips the player's `connected` flag *before* sending the
//! burst, which is why the joiner receives its own id 8.

use plasmo_voice_core::wire::{
    ConnectionPacket, DistanceVisualizePacket, LanguagePacket, LanguageRequestPacket,
    PlayerActivationDistancesPacket, PlayerAudioEndPacket, PlayerDisconnectPacket,
    PlayerInfoPacket, PlayerInfoRequestPacket, PlayerInfoUpdatePacket, PlayerListPacket,
    PlayerStatePacket, SelfSourceInfoPacket, SourceAudioEndPacket, SourceInfoPacket,
    SourceInfoRequestPacket, TcpPacket,
};
use plasmo_voice_core::{PacketDirection, TcpCodec};
use uuid::Uuid;

use crate::config::{PROXIMITY_VISUALIZE_COLOR, ServerConfig, client_version_is_supported};
use crate::server::VoiceServer;

/// The plugin-message channel Plasmo Voice 2.x speaks on.
///
/// This is `BaseVoiceServer.CHANNEL_STRING`, and it is exactly what the client subscribes
/// to: `ModVoiceClient` registers its payload handler for `ModVoiceServer.CHANNEL`
/// (`ModVoiceClient:239-242`). Sending to plain `plasmo:voice` — the 1.x name — produces no
/// error anywhere and no traffic the client will read.
pub const CHANNEL: &str = "plasmo:voice/v2";

/// The pre-2.0 channel name, still accepted on the way in.
///
/// Everything this server *sends* uses [`CHANNEL`]; accepting the old name inbound costs a
/// string comparison and turns "silent client" into "working session" for anything that
/// still speaks it.
pub const LEGACY_CHANNEL: &str = "plasmo:voice";

/// Answers whether a channel name is one this server reads.
#[must_use]
pub fn is_voice_channel(channel: &str) -> bool {
    channel == CHANNEL || channel == LEGACY_CHANNEL
}

/// One control-plane message the caller must deliver over the [`CHANNEL`] channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    /// The player to send to, or `None` for a broadcast.
    ///
    /// Upstream's `broadcast` only ever reaches players whose UDP connection exists
    /// (`hasVoiceChat()`), so the caller must skip players with no connection rather
    /// than writing to everyone on the Minecraft server.
    pub to: Option<Uuid>,
    /// The already-encoded payload: `[packet id][body]`, ready for the channel.
    pub payload: Vec<u8>,
}

impl Outbound {
    /// A message for one player.
    #[must_use]
    pub fn to_player(player: Uuid, payload: Vec<u8>) -> Self {
        Self {
            to: Some(player),
            payload,
        }
    }

    /// A message for every connected voice player.
    #[must_use]
    pub fn broadcast(payload: Vec<u8>) -> Self {
        Self { to: None, payload }
    }
}

/// The control-plane state machine.
///
/// Stateless apart from the codec and the config: everything about who exists lives in
/// [`VoiceServer`], so there is exactly one copy of the truth.
#[derive(Debug, Clone)]
pub struct ControlPlane {
    config: ServerConfig,
    codec: TcpCodec,
}

impl ControlPlane {
    /// Creates a control plane for a server with the given id.
    #[must_use]
    pub fn new(server_id: Uuid) -> Self {
        Self::with_config(ServerConfig::new(server_id))
    }

    /// Creates a control plane carrying an operator-overridden config.
    #[must_use]
    pub fn with_config(config: ServerConfig) -> Self {
        Self {
            config,
            codec: TcpCodec::new(),
        }
    }

    /// The server's voice configuration.
    #[must_use]
    pub const fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Encodes a packet the way `PacketTcpCodec.encode` does: one id byte, then the body.
    #[must_use]
    pub fn encode(&self, packet: &TcpPacket) -> Vec<u8> {
        self.codec.encode(packet).unwrap_or_default()
    }

    /// Phase 1: ask a client to identify itself.
    ///
    /// `PlayerInfoRequestPacket` is empty-bodied, so the payload is a single id byte.
    #[must_use]
    pub fn request_player_info(&self) -> Vec<u8> {
        self.encode(&TcpPacket::PlayerInfoRequest(PlayerInfoRequestPacket))
    }

    /// Handles one `plasmo:voice` payload from a client.
    ///
    /// Always returns the messages to deliver; a malformed or unexpected packet produces
    /// none, which is upstream's behaviour (its handler catches decode failures and only
    /// debug-logs them).
    ///
    /// `name`, `ip` and `port` come from the host: the nick from the player handle, and
    /// the endpoint the client is told to send UDP to — upstream takes that from its
    /// `[host.public]` config, and this plugin uses the address the client itself
    /// connected to, because a guest cannot discover the server's public address.
    pub fn handle(
        &self,
        server: &mut VoiceServer,
        player: Uuid,
        name: &str,
        ip: &str,
        port: u16,
        data: &[u8],
    ) -> Vec<Outbound> {
        let Ok(Some(packet)) = self.codec.decode(data, PacketDirection::Server) else {
            // Unknown id, an id meant for the other direction, or a malformed body: all
            // three are silence upstream. They are *not* all the same failure to us
            // though — a client whose payloads never decode is a codec bug on our side,
            // and a silent drop would hide it behind an unresponsive client.
            server.note_undecodable_payload(player, data.len());
            return Vec::new();
        };

        match packet {
            TcpPacket::PlayerInfo(info) => {
                self.on_player_info(server, player, name, ip, port, &info)
            }
            TcpPacket::PlayerState(state) => self.on_player_state(server, player, &state),
            TcpPacket::PlayerAudioEnd(end) => self.on_audio_end(server, player, &end),
            TcpPacket::PlayerActivationDistances(distances) => {
                self.on_activation_distances(server, player, &distances)
            }
            TcpPacket::SourceInfoRequest(request) => {
                self.on_source_info_request(server, player, &request)
            }
            TcpPacket::LanguageRequest(request) => self.on_language_request(player, &request),
            other => {
                // Everything left in the SERVER direction is a packet this server
                // deliberately does not answer. `decode` already dropped unknown ids and
                // ids registered for the other direction, so the only way to land here is
                // a *new* serverbound packet that somebody added to `plasmo-voice-core`
                // without wiring it up. Reaching this arm in a test is therefore a
                // finding, not a no-op.
                tracing::debug!(
                    %player,
                    id = other.id(),
                    "ignoring a control packet this server does not handle"
                );
                Vec::new()
            }
        }
    }

    /// Phase 2: the client said who it is, so hand it the UDP endpoint to speak to.
    fn on_player_info(
        &self,
        server: &mut VoiceServer,
        player: Uuid,
        name: &str,
        ip: &str,
        port: u16,
        info: &PlayerInfoPacket,
    ) -> Vec<Outbound> {
        // Upstream refuses a client whose major version differs, or that is older than
        // `clientModMinVersion`, and answers neither with a chat message (which a plugin
        // guest cannot send) nor a `ConnectionPacket`.
        if let Err(reason) = client_version_is_supported(&info.version) {
            tracing::warn!(%player, version = %info.version, %reason, "refusing a voice client");
            return Vec::new();
        }

        let nick = name.trim();
        let secret = server.register_player(
            player,
            if nick.is_empty() {
                None
            } else {
                Some(nick.to_string())
            },
        );

        // The client's RSA public key is kept on the registration so the burst that
        // follows can wrap the server-wide AES key for exactly this player. A client
        // that sent no key (or one the crypto layer refuses) falls back to a plaintext
        // config, matching upstream's `encryption == null` path.
        if !info.public_key.is_empty() {
            server.set_player_public_key(player, &info.public_key);
        }
        tracing::info!(
            %player,
            %secret,
            has_public_key = !info.public_key.is_empty(),
            ip,
            port,
            client_version = %info.version,
            minecraft_version = %info.minecraft_version,
            "a voice client identified itself: {player} on {info} (mc {minecraft_version}), \
             told to use UDP {ip}:{port} with secret {secret}",
            info = info.version,
            minecraft_version = info.minecraft_version
        );

        let packet = TcpPacket::Connection(ConnectionPacket {
            secret,
            ip: ip.to_string(),
            port: i32::from(port),
        });
        vec![Outbound::to_player(player, self.encode(&packet))]
    }

    /// `PlayerStatePacket` — no reply, but a state change is broadcast to the others.
    fn on_player_state(
        &self,
        server: &mut VoiceServer,
        player: Uuid,
        state: &PlayerStatePacket,
    ) -> Vec<Outbound> {
        if !server.set_player_state(&player, state.voice_disabled, state.microphone_muted) {
            return Vec::new();
        }
        let Some(info) = server.voice_player_info(&player) else {
            return Vec::new();
        };
        let packet = TcpPacket::PlayerInfoUpdate(PlayerInfoUpdatePacket { player_info: info });
        vec![Outbound::broadcast(self.encode(&packet))]
    }

    /// `PlayerActivationDistancesPacket` — recorded, and the proximity distance is
    /// answered with a `DistanceVisualizePacket` exactly like upstream's
    /// `ProximityServerActivation.onActivationDistanceChange`: the first set is
    /// skipped (the client drew its circle from the activation's default distance),
    /// and every later change is visualized with the fixed default colour.
    fn on_activation_distances(
        &self,
        server: &mut VoiceServer,
        player: Uuid,
        distances: &PlayerActivationDistancesPacket,
    ) -> Vec<Outbound> {
        // The proximity activation is the only one this server offers; upstream
        // checks `activation.getId() != PROXIMITY_ID` and returns.
        let activation = &self.config().activations()[0];
        let was_set = server.activation_distance(&player, activation.id).is_some();
        server.set_activation_distances(&player, distances.distance_by_activation_id.clone());
        if !was_set {
            // The initial set never renders: `PlayerVoiceChat.activate` clears the
            // circle, and the client's own render already matches the default.
            return Vec::new();
        }
        let Some(distance) = server.activation_distance(&player, activation.id) else {
            return Vec::new();
        };
        let packet = TcpPacket::DistanceVisualize(DistanceVisualizePacket {
            radius: distance,
            hex_color: PROXIMITY_VISUALIZE_COLOR,
            position: None,
        });
        vec![Outbound::to_player(player, self.encode(&packet))]
    }

    /// `PlayerAudioEndPacket` — the stream for one activation is over.
    ///
    /// Upstream turns this into a clientbound `SourceAudioEndPacket` (a *control-plane*
    /// packet, id 18) for the same listeners that heard the audio; there is no UDP end
    /// packet. The speaker also gets a `SelfSourceInfoPacket` so its own overlay closes.
    fn on_audio_end(
        &self,
        server: &mut VoiceServer,
        player: Uuid,
        end: &PlayerAudioEndPacket,
    ) -> Vec<Outbound> {
        // A server-muted speaker's end packet is dropped alongside its audio
        // (upstream `PlayerChannelHandler` checks the mute manager before handling
        // the packet at all).
        if crate::mute::is_muted(&player, crate::server::now_ms()) {
            return Vec::new();
        }
        let Some(source_id) = server.source_id_of(&player) else {
            return Vec::new();
        };
        let Some(_) = server.registered_player(&player) else {
            return Vec::new();
        };

        let listeners =
            server.proximity_listener_ids(&player, end.activation_id, i32::from(end.distance));
        let end_packet = TcpPacket::SourceAudioEnd(SourceAudioEndPacket {
            source_id,
            sequence_number: end.sequence_number,
        });
        let bytes = self.encode(&end_packet);

        let mut out: Vec<Outbound> = listeners
            .into_iter()
            .filter(|listener| *listener != player)
            .map(|listener| Outbound::to_player(listener, bytes.clone()))
            .collect();

        // `SelfActivationHelper.onSourceSendPacket`: the speaker is told its own stream
        // ended, with `sequenceNumber = -1` and the activation it was using.
        let self_info = TcpPacket::SelfSourceInfo(SelfSourceInfoPacket {
            source_info: server.self_source_info(&player, end.activation_id),
        });
        out.push(Outbound::to_player(player, self.encode(&self_info)));
        out
    }

    /// `SourceInfoRequestPacket` — answer with the source's info when we know the source.
    fn on_source_info_request(
        &self,
        server: &VoiceServer,
        player: Uuid,
        request: &SourceInfoRequestPacket,
    ) -> Vec<Outbound> {
        let Some(owner) = server.player_for_source_id(&request.source_id) else {
            tracing::trace!(%player, source = %request.source_id, "no such source");
            return Vec::new();
        };
        let Some(source_info) = server.player_source_info(&owner) else {
            return Vec::new();
        };
        let packet = TcpPacket::SourceInfo(SourceInfoPacket { source_info });
        vec![Outbound::to_player(player, self.encode(&packet))]
    }

    /// `LanguageRequestPacket` — upstream answers with its translated strings.
    ///
    /// The table comes from [`crate::language`]: the requested locale, lowercased, with
    /// `en_us` filling anything it does not carry. The client stores it and translates its
    /// own GUI through it, which is the only way the volume tab's source-line label can
    /// read "Proximity" instead of `pv.activation.proximity`.
    fn on_language_request(&self, player: Uuid, request: &LanguageRequestPacket) -> Vec<Outbound> {
        // The client asks for its own locale; `language::client_language` lowercases it,
        // falls back to `en_us` for a locale this server does not ship, and flattens the
        // `client` scope into the `pv.*` keys the client looks up. An empty map here is not
        // "no translations needed" — it is what makes the client print the raw
        // `pv.activation.proximity` in the volume tab.
        let language = crate::language::client_language(&request.language);
        tracing::info!(
            %player,
            requested = %request.language,
            entries = language.len(),
            locales = crate::language::locales().count(),
            "the client asked for the server's translations; replying with {} entries",
            language.len()
        );
        let packet = TcpPacket::Language(LanguagePacket {
            language_name: request.language.clone(),
            language,
        });
        vec![Outbound::to_player(player, self.encode(&packet))]
    }

    /// Phase 3: the player's UDP connection exists — send the registration burst.
    ///
    /// Upstream's order is `sendConfigInfo` → `sendPlayerList` →
    /// `broadcastPlayerInfoUpdate`, and the broadcast includes the joiner because its
    /// `connected` flag was already set.
    ///
    /// A player who registered but never opened a UDP connection gets **nothing**: the
    /// burst is what the *connection* triggers, and there is no client to receive it.
    #[must_use]
    pub fn registration_burst(&self, server: &VoiceServer, player: Uuid) -> Vec<Outbound> {
        if server.client_by_player(&player).is_none() {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(3);

        // 1. `sendConfigInfo` — the whole server config, with the AES key wrapped
        //    for this player. A client that sent a public key this server cannot
        //    parse gets `encryption: None` — upstream aborts the config packet
        //    entirely on that failure, this server downgrades loudly instead, so
        //    the client never receives a key it cannot use.
        let encryption = server.player_public_key(&player).and_then(|der| {
            match server.aes_key().encryption_info(der) {
                Ok(info) => Some(info),
                Err(error) => {
                    tracing::warn!(
                        %player,
                        %error,
                        "the client's RSA public key is unusable; its audio stays plaintext"
                    );
                    None
                }
            }
        });
        let config = TcpPacket::Config(self.config.config_packet(encryption));
        out.push(Outbound::to_player(player, self.encode(&config)));

        // 2. `sendPlayerList` — everyone with a live connection, including this player.
        let players = server
            .connected_player_ids()
            .into_iter()
            .filter_map(|id| server.voice_player_info(&id))
            .collect();
        let list = TcpPacket::PlayerList(PlayerListPacket { players });
        out.push(Outbound::to_player(player, self.encode(&list)));

        // 3. `broadcastPlayerInfoUpdate` — tell everyone this player is now speaking.
        if let Some(info) = server.voice_player_info(&player) {
            let update = TcpPacket::PlayerInfoUpdate(PlayerInfoUpdatePacket { player_info: info });
            out.push(Outbound::broadcast(self.encode(&update)));
        }

        out
    }

    /// A UDP connection went away (timeout, or the client replaced it).
    ///
    /// Upstream broadcasts `PlayerDisconnectPacket` and keeps the *registration*: the
    /// secret is sticky, so the same client can come back with the same secret.
    #[must_use]
    pub fn disconnected(&self, player: Uuid) -> Vec<Outbound> {
        let packet = TcpPacket::PlayerDisconnect(PlayerDisconnectPacket { player_id: player });
        vec![Outbound::broadcast(self.encode(&packet))]
    }

    /// A player left the Minecraft server: forget the registration and announce it.
    pub fn disconnect(&self, server: &mut VoiceServer, player: Uuid) -> Vec<Outbound> {
        if server.unregister_player(&player).is_none() {
            return Vec::new();
        }
        tracing::info!(%player, "a voice player left");
        self.disconnected(player)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plasmo_voice_core::wire::{PingPacket, PlayerStatePacket, SourceAudioEndPacket};
    use plasmo_voice_core::{TcpPacket, UdpCodec, UdpPacket};

    const SERVER_ID: Uuid = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
    const ALICE: Uuid = Uuid::from_u128(0x0000_000a_11ce);
    const BOB: Uuid = Uuid::from_u128(0x0000_0000_0b0b);

    /// A fixed server-wide AES key, so tests stay deterministic and the data
    /// plane does not touch the guest's random source.
    fn test_aes_key() -> crate::crypto::AesKey {
        crate::crypto::AesKey::from_hex("00112233445566778899aabbccddeeff")
            .expect("the test key is 32 lowercase hex chars")
    }

    /// The server under test, on a fixed secret.
    fn server() -> VoiceServer {
        VoiceServer::new(SERVER_ID, test_aes_key())
    }

    fn codec() -> TcpCodec {
        TcpCodec::new()
    }

    /// The channel name is upstream's, letter for letter.
    ///
    /// This is not cosmetic: the client registers its receiver on the exact string, and a
    /// mismatch is invisible from both ends — the server sends happily, the client ignores
    /// happily, and the user is told the server has no voice plugin. Hence a pinned
    /// assertion rather than a comment.
    #[test]
    fn the_voice_channel_is_the_one_the_client_listens_on() {
        assert_eq!(CHANNEL, "plasmo:voice/v2");
        assert_eq!(LEGACY_CHANNEL, "plasmo:voice");
        assert!(is_voice_channel(CHANNEL));
        assert!(
            is_voice_channel(LEGACY_CHANNEL),
            "the pre-2.0 name is still read on the way in"
        );
        assert!(!is_voice_channel("plasmo:voice/v3"));
        assert!(!is_voice_channel("plasmo:voice/v2/installed"));
        assert!(
            !is_voice_channel("minecraft:register"),
            "other plugins' traffic must not be treated as voice"
        );
    }

    /// A `PlayerInfoPacket` as a healthy client sends one.
    ///
    /// The public key must be non-empty: the codec enforces upstream's
    /// `readSafeInt(in, 1, 2048)` on it, and a real client always ships a fresh RSA key
    /// (it generates one at startup whether or not the server asks for encryption).
    fn player_info(version: &str) -> Vec<u8> {
        codec()
            .encode(&TcpPacket::PlayerInfo(PlayerInfoPacket {
                voice_disabled: false,
                microphone_muted: false,
                minecraft_version: "1.21.4".to_string(),
                version: version.to_string(),
                public_key: vec![0x30, 0x82, 0x01, 0x22, 0x00, 0x01, 0x02, 0x03],
            }))
            .expect("encode")
    }

    /// The leading id byte of a payload, which is the whole of the framing.
    fn id_of(payload: &[u8]) -> u8 {
        *payload.first().expect("every payload carries an id")
    }

    /// Decodes a payload the way the client does.
    fn decode(payload: &[u8]) -> TcpPacket {
        codec()
            .decode(payload, PacketDirection::Client)
            .expect("decode ok")
            .expect("a registered clientbound id")
    }

    /// Registers a player and opens its UDP connection through a real datagram.
    fn connect(server: &mut VoiceServer, player: Uuid, address: &str) -> Uuid {
        let secret = server.register_player(player, Some("someone".to_string()));
        let ping = UdpCodec::new()
            .encode(&UdpPacket::Ping(PingPacket::new(None, 0)), secret, 1)
            .expect("encode");
        let handled = server.handle_datagram(address, &ping);
        assert_eq!(handled.control.len(), 3, "the registration burst");
        secret
    }

    #[test]
    fn a_client_that_identifies_itself_is_given_its_udp_endpoint() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();

        let messages = plane.handle(
            &mut server,
            ALICE,
            "Alice",
            "0.0.0.0",
            8830,
            &player_info("2.1.7"),
        );

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].to, Some(ALICE));
        assert_eq!(id_of(&messages[0].payload), 1, "ConnectionPacket");
        match decode(&messages[0].payload) {
            TcpPacket::Connection(connection) => {
                assert_eq!(connection.secret, server.secret_for_player(&ALICE).unwrap());
                assert_eq!(connection.ip, "0.0.0.0");
                assert_eq!(connection.port, 8830);
            }
            other => panic!("expected a connection packet, got {other:?}"),
        }
        assert_eq!(server.registered_player_count(), 1);
        assert_eq!(server.player_name(&ALICE), Some("Alice"));
    }

    #[test]
    fn an_incompatible_client_is_refused_silently() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();

        // A major-version mismatch, an ancient client, and a version that is not one.
        for version in ["3.0.0", "1.9.9", "garbage"] {
            let messages = plane.handle(
                &mut server,
                ALICE,
                "Alice",
                "0.0.0.0",
                8830,
                &player_info(version),
            );
            assert!(messages.is_empty(), "{version} must be refused");
        }
        assert_eq!(
            server.registered_player_count(),
            0,
            "a refused client is never registered, so its secret is never minted"
        );
    }

    #[test]
    fn undecodable_and_misdirected_payloads_are_ignored() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();

        // A truncated packet, an unknown id, and a *clientbound* id sent by the client.
        let clientbound = codec()
            .encode(&TcpPacket::PlayerList(PlayerListPacket {
                players: Vec::new(),
            }))
            .expect("encode");
        for payload in [&b"\x0a\x00"[..], &b"\xfe"[..], &clientbound[..]] {
            let messages = plane.handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, payload);
            assert!(messages.is_empty(), "payload {payload:?} must be ignored");
        }
        assert_eq!(server.registered_player_count(), 0);
    }

    #[test]
    fn a_state_change_is_broadcast_but_a_repeat_is_not() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();
        connect(&mut server, ALICE, "1.1.1.1:10");

        let payload = codec()
            .encode(&TcpPacket::PlayerState(PlayerStatePacket {
                voice_disabled: true,
                microphone_muted: false,
            }))
            .expect("encode");
        let messages = plane.handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &payload);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].to, None, "a broadcast");
        assert_eq!(id_of(&messages[0].payload), 8, "PlayerInfoUpdatePacket");
        match decode(&messages[0].payload) {
            TcpPacket::PlayerInfoUpdate(update) => {
                assert_eq!(update.player_info.player_id, ALICE);
                assert!(update.player_info.voice_disabled);
            }
            other => panic!("expected a player-info update, got {other:?}"),
        }

        // The same state again changes nothing, so nothing is broadcast.
        let again = plane.handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &payload);
        assert!(again.is_empty());
    }

    #[test]
    fn activation_distances_are_recorded_and_answered_from_the_second_set() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();
        connect(&mut server, ALICE, "1.1.1.1:10");

        let proximity = plane.config().activations()[0].id;
        let unknown = Uuid::from_u128(0xbeef);
        let payload = codec()
            .encode(&TcpPacket::PlayerActivationDistances(
                PlayerActivationDistancesPacket {
                    distance_by_activation_id: vec![(proximity, 32), (unknown, 8)],
                },
            ))
            .expect("encode");

        let messages = plane.handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &payload);
        assert!(
            messages.is_empty(),
            "the first set is never answered, like upstream's onActivationDistanceChange"
        );

        let stored = server
            .registered_player(&ALICE)
            .expect("registered")
            .activation_distances
            .clone();
        assert_eq!(
            stored,
            vec![(proximity, 32)],
            "an activation this server does not offer is dropped"
        );

        // A *change* is visualized with the default colour, addressed to the player.
        let changed = codec()
            .encode(&TcpPacket::PlayerActivationDistances(
                PlayerActivationDistancesPacket {
                    distance_by_activation_id: vec![(proximity, 16)],
                },
            ))
            .expect("encode");
        let messages = plane.handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &changed);
        assert_eq!(messages.len(), 1, "a later change draws a new circle");
        assert_eq!(messages[0].to, Some(ALICE), "directed at the player");
        match decode(&messages[0].payload) {
            TcpPacket::DistanceVisualize(visualize) => {
                assert_eq!(visualize.radius, 16);
                assert_eq!(visualize.hex_color, PROXIMITY_VISUALIZE_COLOR);
                assert_eq!(visualize.position, None);
            }
            other => panic!("expected a DistanceVisualizePacket, got {other:?}"),
        }
    }

    #[test]
    fn an_audio_end_reaches_the_listeners_and_the_speaker() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();
        connect(&mut server, ALICE, "1.1.1.1:10");
        connect(&mut server, BOB, "2.2.2.2:20");
        server.set_position(&ALICE, Some("world".to_string()), (0.0, 64.0, 0.0));
        server.set_position(&BOB, Some("world".to_string()), (4.0, 64.0, 0.0));
        let proximity = plane.config().activations()[0].id;

        let payload = codec()
            .encode(&TcpPacket::PlayerAudioEnd(PlayerAudioEndPacket {
                sequence_number: 42,
                activation_id: proximity,
                distance: 16,
            }))
            .expect("encode");
        let messages = plane.handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &payload);

        assert_eq!(messages.len(), 2, "bob plus alice");
        let to_bob = messages
            .iter()
            .find(|message| message.to == Some(BOB))
            .expect("bob was listening");
        assert_eq!(id_of(&to_bob.payload), 18, "SourceAudioEndPacket");
        match decode(&to_bob.payload) {
            TcpPacket::SourceAudioEnd(end) => {
                let expected = SourceAudioEndPacket {
                    source_id: server.source_id_of(&ALICE).unwrap(),
                    sequence_number: 42,
                };
                assert_eq!(end, expected);
            }
            other => panic!("expected a source-audio end, got {other:?}"),
        }

        let to_alice = messages
            .iter()
            .find(|message| message.to == Some(ALICE))
            .expect("the speaker is told its own stream ended");
        assert_eq!(id_of(&to_alice.payload), 17, "SelfSourceInfoPacket");
        match decode(&to_alice.payload) {
            TcpPacket::SelfSourceInfo(info) => {
                assert_eq!(info.source_info.player_id, ALICE);
                assert_eq!(info.source_info.activation_id, proximity);
                assert_eq!(
                    info.source_info.sequence_number, -1,
                    "upstream sends -1 for 'this stream is over'"
                );
            }
            other => panic!("expected self source info, got {other:?}"),
        }
    }

    #[test]
    fn a_source_info_request_is_answered_for_a_known_source_only() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();
        connect(&mut server, ALICE, "1.1.1.1:10");
        connect(&mut server, BOB, "2.2.2.2:20");

        let alice_source = server.source_id_of(&ALICE).expect("a source");
        let payload = codec()
            .encode(&TcpPacket::SourceInfoRequest(SourceInfoRequestPacket {
                source_id: alice_source,
            }))
            .expect("encode");
        let messages = plane.handle(&mut server, BOB, "Bob", "0.0.0.0", 8830, &payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].to, Some(BOB));
        assert_eq!(id_of(&messages[0].payload), 16, "SourceInfoPacket");
        match decode(&messages[0].payload) {
            TcpPacket::SourceInfo(info) => match info.source_info {
                plasmo_voice_core::data::SourceInfo::Player(source) => {
                    assert_eq!(source.base.id, alice_source);
                    assert_eq!(source.player_info.player_id, ALICE);
                }
                other => panic!("expected a player source, got {other:?}"),
            },
            other => panic!("expected source info, got {other:?}"),
        }

        // A source nobody publishes is not answered at all.
        let payload = codec()
            .encode(&TcpPacket::SourceInfoRequest(SourceInfoRequestPacket {
                source_id: Uuid::from_u128(0xdead),
            }))
            .expect("encode");
        assert!(
            plane
                .handle(&mut server, BOB, "Bob", "0.0.0.0", 8830, &payload)
                .is_empty()
        );
    }

    #[test]
    fn a_language_request_is_answered_with_the_requested_locale() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();

        let payload = codec()
            .encode(&TcpPacket::LanguageRequest(LanguageRequestPacket {
                language: "ru_ru".to_string(),
            }))
            .expect("encode");
        let messages = plane.handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &payload);

        assert_eq!(messages.len(), 1);
        assert_eq!(id_of(&messages[0].payload), 6, "LanguagePacket");
        match decode(&messages[0].payload) {
            TcpPacket::Language(language) => {
                assert_eq!(language.language_name, "ru_ru");
                // The requested locale is echoed back, but what matters is the table: it is
                // answered in that locale, and it is what the volume tab's source-line
                // label is rendered from.
                assert_eq!(
                    language.language,
                    vec![(
                        "pv.activation.proximity".to_string(),
                        "Локальный".to_string()
                    )],
                    "the proximity translation is what the volume tab renders"
                );
            }
            other => panic!("expected a language packet, got {other:?}"),
        }
    }

    /// A locale this server does not ship is answered in the fallback locale, never with an
    /// empty table — an empty table is what puts the raw `pv.activation.proximity` on the
    /// client's screen.
    #[test]
    fn an_unknown_locale_is_answered_in_the_fallback_locale() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();

        let payload = codec()
            .encode(&TcpPacket::LanguageRequest(LanguageRequestPacket {
                language: "xx_yy".to_string(),
            }))
            .expect("encode");
        let messages = plane.handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &payload);

        match decode(&messages[0].payload) {
            TcpPacket::Language(language) => {
                assert_eq!(language.language_name, "xx_yy");
                assert_eq!(
                    language.language,
                    vec![(
                        "pv.activation.proximity".to_string(),
                        "Proximity".to_string()
                    )]
                );
            }
            other => panic!("expected a language packet, got {other:?}"),
        }
    }

    #[test]
    fn the_burst_needs_a_registration_and_a_connection() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();
        assert!(
            plane.registration_burst(&server, ALICE).is_empty(),
            "a stranger gets nothing"
        );

        let secret = server.register_player(ALICE, Some("Alice".into()));
        assert!(
            plane.registration_burst(&server, ALICE).is_empty(),
            "registered but never connected is still nothing: the burst is what a UDP \
             connection triggers"
        );

        server.add_connection(secret, "1.1.1.1:10".to_string(), Some(ALICE));
        let burst = plane.registration_burst(&server, ALICE);
        assert_eq!(burst.len(), 3);
        assert_eq!(id_of(&burst[0].payload), 3, "ConfigPacket");
        assert_eq!(burst[0].to, Some(ALICE));
        assert_eq!(id_of(&burst[1].payload), 7, "PlayerListPacket");
        assert_eq!(id_of(&burst[2].payload), 8, "PlayerInfoUpdatePacket");
        assert_eq!(burst[2].to, None);
    }

    #[test]
    fn disconnecting_forgets_the_player_and_broadcasts_it() {
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();
        connect(&mut server, ALICE, "1.1.1.1:10");

        let messages = plane.disconnect(&mut server, ALICE);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].to, None);
        assert_eq!(id_of(&messages[0].payload), 9, "PlayerDisconnectPacket");
        assert_eq!(server.registered_player_count(), 0);
        assert_eq!(server.connection_count(), 0);

        // A player who was never registered produces nothing.
        assert!(plane.disconnect(&mut server, ALICE).is_empty());

        // A *connection* that goes away keeps the registration: only the connection died.
        let after_timeout = plane.disconnected(ALICE);
        assert_eq!(after_timeout.len(), 1);
        assert_eq!(id_of(&after_timeout[0].payload), 9);
    }

    #[test]
    fn every_serverbound_packet_has_an_arm() {
        // The registry is the only place that knows the full SERVER-direction set: exactly
        // six ids, and `ControlPlane::handle` must have an arm for each. A new serverbound
        // packet added to the core crate fails here until it is classified — that is the
        // point, because the `other` arm in `handle` is otherwise silent.
        const HANDLED: [u8; 6] = [
            0x05, // LanguageRequest
            0x0A, // PlayerInfo
            0x0B, // PlayerState
            0x0C, // PlayerAudioEnd
            0x0D, // PlayerActivationDistances
            0x0F, // SourceInfoRequest
        ];

        let tcp = codec();
        let registry = tcp.registry();
        let mut serverbound: Vec<u8> = (0u16..=0xFF)
            .map(|id| id as u8)
            .filter(|id| {
                registry
                    .tcp_by_type(u32::from(*id), PacketDirection::Server)
                    .is_some()
            })
            .collect();
        serverbound.sort_unstable();

        let mut expected = HANDLED.to_vec();
        expected.sort_unstable();
        assert_eq!(
            serverbound, expected,
            "the serverbound id set changed; add an arm in `ControlPlane::handle` and \
             update this list"
        );

        // Every one of them reaches a real arm: none of these six calls may be answered
        // with nothing, except the two that are legitimately silent.
        let plane = ControlPlane::new(SERVER_ID);
        let mut server = server();
        connect(&mut server, ALICE, "1.1.1.1:10");

        assert_eq!(
            plane
                .handle(
                    &mut server,
                    ALICE,
                    "Alice",
                    "0.0.0.0",
                    8830,
                    &player_info("2.1.7")
                )
                .len(),
            1,
            "0x0A PlayerInfo is the handshake"
        );
        let state = codec()
            .encode(&TcpPacket::PlayerState(PlayerStatePacket {
                voice_disabled: true,
                microphone_muted: false,
            }))
            .expect("encode");
        assert_eq!(
            plane
                .handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &state)
                .len(),
            1,
            "0x0B PlayerState broadcasts the change"
        );
        let language = codec()
            .encode(&TcpPacket::LanguageRequest(LanguageRequestPacket {
                language: "en_us".to_string(),
            }))
            .expect("encode");
        assert_eq!(
            plane
                .handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &language)
                .len(),
            1,
            "0x05 LanguageRequest is answered"
        );
        let distances = codec()
            .encode(&TcpPacket::PlayerActivationDistances(
                PlayerActivationDistancesPacket {
                    distance_by_activation_id: Vec::new(),
                },
            ))
            .expect("encode");
        assert!(
            plane
                .handle(&mut server, ALICE, "Alice", "0.0.0.0", 8830, &distances)
                .is_empty(),
            "0x0D PlayerActivationDistances is recorded; the first set is never answered"
        );

        // The other two are exercised by the dedicated tests above; assert here only that
        // their ids are the ones this test classifies.
        assert!(HANDLED.contains(&0x0C) && HANDLED.contains(&0x0F));
    }

    #[test]
    fn a_config_packet_round_trips_in_the_clientbound_direction_only() {
        let plane = ControlPlane::new(SERVER_ID);
        let config = plane.config().config_packet(None);
        let bytes = codec()
            .encode(&TcpPacket::Config(config.clone()))
            .expect("encode");
        assert_eq!(id_of(&bytes), 3);
        assert_eq!(decode(&bytes), TcpPacket::Config(config));
        // Serverbound direction must refuse a clientbound id.
        assert!(
            codec()
                .decode(&bytes, PacketDirection::Server)
                .expect("decode ok")
                .is_none()
        );
    }
}
