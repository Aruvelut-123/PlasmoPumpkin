//! The server's `ConfigPacket` — everything a client needs before it can speak.
//!
//! Upstream builds this in `VoiceTcpServerConnectionManager.sendConfigInfo`
//! (`server/common/src/main/java/su/plo/voice/server/connection/VoiceTcpServerConnectionManager.java:97`)
//! out of `VoiceServerConfig` plus the activation and source-line registries. This plugin
//! ships the upstream **defaults**, and every constant below names the config key it stands
//! in for.
//!
//! The wire-facing [`ServerConfig`] is fixed at build time; the few server-runner knobs
//! that do **not** go on the wire (`port`, `keep_alive_timeout_ms`, `advertised_ip`,
//! datagram budget) are read from a flat `config.toml` in the data folder by
//! [`PluginConfig::load`].
//!
//! ## What the client actually requires (verified in the client, not guessed)
//!
//! * `serverId` — its config key and server identity.
//! * `captureInfo.encoderInfo` — `VoiceServerInfo.createOpusEncoder` throws
//!   `"server codec info is empty"` when it is null, so it is **mandatory**.
//! * `playerIconConfig` — the client dereferences it unconditionally
//!   (`config.getPlayerIconConfig().getIconVisibility()`), so a config without it NPEs on
//!   a real client. An empty visibility list is fine.
//! * `activations` — legally empty, but with no activation the client cannot key up, so
//!   the proximity activation is required for a *usable* server.
//! * `encryption` — per-player, and genuinely optional: `null` means "plaintext audio",
//!   which the client accepts. Defaults to a real `EncryptionInfo` here (see
//!   [`ServerConfig::config_packet`]); `None` only survives when the client sent no
//!   usable public key.

use plasmo_voice_core::data::{
    CaptureInfo, CodecInfo, EncryptionInfo, PlayerIconConfig, Pos3d, VoiceActivation,
    VoiceSourceLine,
};
use plasmo_voice_core::wire::ConfigPacket;
use uuid::Uuid;

/// `voice.sampleRate` default (`VoiceServerConfig.java:140`).
pub const SAMPLE_RATE: i32 = 48_000;

/// `voice.mtuSize` default (`VoiceServerConfig.java:154`), valid range 128..=5000.
pub const MTU_SIZE: i32 = 1024;

/// `voice.opus().mode` default (`VoiceServerConfig.java:270`).
pub const OPUS_MODE: &str = "VOIP";

/// `voice.opus().bitrate` default: `-1000` means "auto" upstream.
pub const OPUS_BITRATE: &str = "-1000";

/// `voice.proximity().distances` default, ascending (`VoiceServerConfig.java:250`).
pub const PROXIMITY_DISTANCES: [i32; 3] = [8, 16, 32];

/// `voice.proximity().defaultDistance` default (`VoiceServerConfig.java:250`).
pub const PROXIMITY_DEFAULT_DISTANCE: i32 = 16;

/// `voice.maxExtraAudioBroadcastDistance` default, part of the relay radius
/// (`VoiceServerConfig.java:137`).
pub const MAX_EXTRA_AUDIO_BROADCAST_DISTANCE: i32 = 16;

/// `ProximityServerActivation` translation key (`ProximityServerActivation.kt:30`).
pub const PROXIMITY_TRANSLATION: &str = "pv.activation.proximity";

/// `ProximityServerActivation` activation icon.
pub const PROXIMITY_ACTIVATION_ICON: &str = "plasmovoice:textures/icons/microphone.png";

/// `ProximityServerActivation` source-line icon.
pub const PROXIMITY_LINE_ICON: &str = "plasmovoice:textures/icons/speaker.png";

/// Weight of the built-in proximity activation and line (`ProximityServerActivation.kt:33`).
pub const PROXIMITY_WEIGHT: i32 = 1;

/// The only permission upstream registers by default, granted to everyone
/// (`BaseVoiceServer.java:147`, `Permissions.kt:20`).
pub const ALLOW_FREECAM: &str = "pv.allow_freecam";

/// The major protocol version this server speaks.
///
/// `PlayerChannelHandler.handle(PlayerInfoPacket)` refuses a client whose *major* version
/// differs, and the check is on the client's reported mod version, not on the wire
/// protocol constant.
pub const PROTOCOL_MAJOR: u64 = 2;

/// `voice.clientModMinVersion` default (`VoiceServerConfig.java:168`).
pub const MIN_CLIENT_VERSION: &str = "2.0.0";

/// `ProximityServerActivation` as it goes on the wire.
///
/// `id` is derived from the name, never transmitted: `generateId("proximity")` is the MD5
/// name-UUID of `"proximity_activation"` (`VoiceActivation.java:29`).
pub fn proximity_activation() -> VoiceActivation {
    VoiceActivation {
        id: VoiceActivation::generate_id(VoiceActivation::PROXIMITY_NAME),
        name: VoiceActivation::PROXIMITY_NAME.to_string(),
        translation: PROXIMITY_TRANSLATION.to_string(),
        icon: PROXIMITY_ACTIVATION_ICON.to_string(),
        distances: PROXIMITY_DISTANCES.to_vec(),
        default_distance: PROXIMITY_DEFAULT_DISTANCE,
        proximity: true,
        transitive: true,
        stereo_supported: false,
        encoder_info: None,
        weight: PROXIMITY_WEIGHT,
    }
}

/// The `"proximity"` source line (`ProximityServerActivation.kt:39`).
///
/// `players` stays `None`: upstream only fills it for lines that have a player-set
/// manager, and the built-in proximity line does not.
pub fn proximity_source_line() -> VoiceSourceLine {
    VoiceSourceLine {
        id: VoiceSourceLine::generate_id(VoiceSourceLine::PROXIMITY_NAME),
        name: VoiceSourceLine::PROXIMITY_NAME.to_string(),
        translation: PROXIMITY_TRANSLATION.to_string(),
        icon: PROXIMITY_LINE_ICON.to_string(),
        default_volume: 1.0,
        weight: PROXIMITY_WEIGHT,
        players: None,
    }
}

/// `voice.capture` as it goes on the wire.
pub fn capture_info() -> CaptureInfo {
    CaptureInfo {
        sample_rate: SAMPLE_RATE,
        mtu_size: MTU_SIZE,
        encoder_info: Some(CodecInfo {
            name: "opus".to_string(),
            params: vec![
                ("mode".to_string(), OPUS_MODE.to_string()),
                ("bitrate".to_string(), OPUS_BITRATE.to_string()),
            ],
        }),
    }
}

/// The server's voice configuration, as far as the wire is concerned.
///
/// Deliberately tiny: everything a client is told is derived from the server id it is
/// paired with, so there is exactly one value that can drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    server_id: Uuid,
}

impl ServerConfig {
    /// Pairs a configuration with the persisted server id.
    #[must_use]
    pub fn new(server_id: Uuid) -> Self {
        Self { server_id }
    }

    /// `ConfigPacket.serverId`.
    #[must_use]
    pub fn server_id(&self) -> Uuid {
        self.server_id
    }

    /// The activation catalogue this server offers.
    #[must_use]
    pub fn activations(&self) -> Vec<VoiceActivation> {
        vec![proximity_activation()]
    }

    /// The source-line catalogue this server offers.
    #[must_use]
    pub fn source_lines(&self) -> Vec<VoiceSourceLine> {
        vec![proximity_source_line()]
    }

    /// The permissions map sent to a player.
    ///
    /// Upstream sends every permission the server knows about paired with that player's
    /// value; only `pv.allow_freecam` exists by default and it defaults to true.
    #[must_use]
    pub fn permissions(&self) -> Vec<(String, bool)> {
        vec![(ALLOW_FREECAM.to_string(), true)]
    }

    /// `VoiceTcpServerConnectionManager.sendConfigInfo`: the packet a player is sent once
    /// their UDP connection exists.
    ///
    /// ## Encryption
    ///
    /// Upstream RSA-encrypts a 16-byte AES key with the public key the client sent in its
    /// `PlayerInfoPacket` (`VoiceTcpServerConnectionManager.java:110`) and lets a failure
    /// abort the config packet entirely. This plugin wraps the key in [`crate::crypto`]
    /// and hands the result in through `encryption`: the control plane computes it per
    /// player at registration time, and passes `None` when the client sent no usable key
    /// so the client falls back to plaintext audio exactly like upstream's `null`.
    #[must_use]
    pub fn config_packet(&self, encryption: Option<EncryptionInfo>) -> ConfigPacket {
        ConfigPacket {
            server_id: self.server_id,
            capture_info: capture_info(),
            encryption,
            source_lines: self.source_lines(),
            activations: self.activations(),
            permissions: self.permissions(),
            // An empty visibility list means "show every icon"; upstream's default is
            // `PlayerIconVisibility.none()`, i.e. empty, with a y-offset of 0.0.
            player_icon_config: Some(PlayerIconConfig::new(Vec::new(), Pos3d::zero())),
        }
    }
}

/// A `major.minor.patch` triple, for the two version gates upstream applies to a client's
/// `PlayerInfoPacket`.
///
/// Upstream parses with `SemanticVersion`; anything unparsable is treated as incompatible
/// there too (it throws, the handler's catch logs, and no `ConnectionPacket` is sent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parses `"2.1.7"`, ignoring any pre-release or build suffix.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let core = text
            .trim()
            .split(['-', '+'])
            .next()
            .unwrap_or_default()
            .to_string();
        let mut parts = core.split('.');
        let major = parts.next()?.trim().parse().ok()?;
        let minor = parts.next().unwrap_or("0").trim().parse().ok()?;
        let patch = parts.next().unwrap_or("0").trim().parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// `PlayerChannelHandler.handle(PlayerInfoPacket)`: may this client be served?
///
/// Returns `Err` with the reason to log; upstream sends a chat suggestion to the player in
/// both rejection cases and never replies with a `ConnectionPacket`.
pub fn client_version_is_supported(client_version: &str) -> Result<Version, String> {
    let Some(client) = Version::parse(client_version) else {
        return Err(format!(
            "the client reported version {client_version:?}, which is not a semantic version"
        ));
    };
    if client.major != PROTOCOL_MAJOR {
        return Err(format!(
            "client version {client} does not match server major version {PROTOCOL_MAJOR}"
        ));
    }
    let min = Version::parse(MIN_CLIENT_VERSION).unwrap_or(Version {
        major: PROTOCOL_MAJOR,
        minor: 0,
        patch: 0,
    });
    if client < min {
        return Err(format!(
            "client version {client} is older than the minimum {min}"
        ));
    }
    Ok(client)
}

/// `Activation.checkDistance`: is `distance` one the activation allows?
#[must_use]
pub fn check_distance(activation: &VoiceActivation, distance: i32) -> bool {
    if activation.distances.is_empty() {
        return true;
    }
    if activation.distances.len() == 2 && activation.distances[0] == -1 {
        return distance >= 1 && distance <= activation.distances[1];
    }
    activation.distances.contains(&distance)
}

/// `Activation.calculateAllowedDistance`: the client's request, or the activation's
/// default when the request is not one this activation permits.
#[must_use]
pub fn calculate_allowed_distance(activation: &VoiceActivation, requested: i32) -> i32 {
    if check_distance(activation, requested) {
        requested
    } else {
        activation.default_distance
    }
}

/// The plugin's own `config.toml` file name, in the data folder.
pub const CONFIG_FILE: &str = "config.toml";

/// The commented template written out on first run, so an operator sees every
/// knob the server reads without hunting through source. Every key here is
/// optional; the flat parser ignores comments, blanks, and unknown keys.
pub const DEFAULT_CONFIG_TOML: &str = "# Plasmo Voice server configuration
# Every key is optional: delete the ones you want to keep the default.
#
# UDP port the voice server listens on. 0 means \"trust state.toml\": the first
# run asks the host for an ephemeral port and remembers it, and reloads keep
# listening on that same port. Set this to pin the port permanently.
# port = 24424
#
# How long a connection may stay silent before it is retired, in milliseconds
# (upstream: voice.keepAliveTimeoutMs, default 15000).
# keep_alive_timeout_ms = 15000
#
# The IP advertised to clients in ConnectionPacket.ip (upstream: [host].public
# ip). The default, 0.0.0.0, makes the official client use the Minecraft host it
# is already connected to, which is right for a voice server on the same machine.
# Point it at a public address or domain only when the voice port is elsewhere.
# advertised_ip = \"0.0.0.0\"
#
# Datagrams drained from the UDP socket per server tick. This is the rate limit
# that keeps a flood from stalling the tick; anything beyond it is dropped by the
# OS buffer, exactly like an overloaded real-time server (default 256).
# max_datagrams_per_tick = 256
";

/// Server-runner settings read from `config.toml` in the data folder.
///
/// Unlike [`ServerConfig`] — which is what the *client* is told and is fixed at
/// build time — these are the knobs that decide how the voice server itself runs:
/// which UDP port it binds, how long a silent connection is kept alive, which IP it
/// advertises, and the per-tick datagram budget (the rate limit that keeps a flood
/// from stalling a server tick). Every key is optional; a missing key keeps the
/// upstream default, and unknown keys are ignored, so a config file can be
/// forward-compatible across plugin versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfig {
    /// UDP port to bind. `0` (the default) means "use whatever `state.toml`
    /// records", which is the historical behaviour: first run picks an ephemeral
    /// port, later runs keep listening on it. A non-zero value here wins over the
    /// state file.
    pub port: u16,
    /// How long a connection may stay silent before it is retired, mirroring
    /// `VoiceServerConfig.keepAliveTimeoutMs` (`voice.keepAliveTimeoutMs`),
    /// default 15_000.
    pub keep_alive_timeout_ms: u64,
    /// The IP advertised in `ConnectionPacket.ip` (`[host].public ip` upstream),
    /// default `0.0.0.0` — which the official client resolves to its Minecraft host.
    pub advertised_ip: String,
    /// Datagrams drained from the socket per tick, mirroring the fixed budget in
    /// `runtime::MAX_DATAGRAMS_PER_TICK` (itself the upstream-equivalent rate
    /// limit), default 256.
    pub max_datagrams_per_tick: usize,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            port: 0,
            keep_alive_timeout_ms: crate::server::KEEP_ALIVE_TIMEOUT_MS,
            advertised_ip: "0.0.0.0".to_string(),
            max_datagrams_per_tick: crate::runtime::MAX_DATAGRAMS_PER_TICK,
        }
    }
}

impl PluginConfig {
    /// Writes the commented default [`DEFAULT_CONFIG_TOML`] template into `folder`
    /// on first run, unless the file already exists. Mirrors how `state.toml` is
    /// created; an operator who already dropped in a `config.toml` is never
    /// overwritten.
    ///
    /// # Errors
    ///
    /// Fails when the data folder is not writable, which the caller must surface —
    /// silently skipping the template would leave the operator without a config file
    /// to edit.
    pub fn create_default_if_missing(folder: &str) -> Result<bool, String> {
        let path = format!("{folder}/{CONFIG_FILE}");
        if std::path::Path::new(&path).exists() {
            return Ok(false);
        }
        std::fs::write(&path, DEFAULT_CONFIG_TOML).map_err(|error| {
            format!("could not write the default voice config to {path}: {error}")
        })?;
        tracing::info!(%path, "wrote the default voice config template");
        Ok(true)
    }

    /// Reads `config.toml` from `folder`, falling back to defaults when the file is
    /// missing or unreadable (a warn is logged either way; a bad *value* is not fatal).
    #[must_use]
    pub fn load(folder: &str) -> Self {
        let path = format!("{folder}/{CONFIG_FILE}");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_toml(&text).unwrap_or_else(|error| {
                tracing::warn!(
                    %path,
                    %error,
                    "ignoring unreadable voice config; using defaults"
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Parses a flat `key = value` TOML document, the same shape `state.toml` uses.
    ///
    /// # Errors
    ///
    /// Fails only when a *known* key has an un-parseable value; malformed lines are
    /// skipped by the shared flat parser, and unknown keys are left untouched.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let mut config = Self::default();
        let mut unknown = Vec::new();
        for (key, value) in crate::state::parse_flat_table(text)? {
            match key.as_str() {
                "port" => {
                    config.port = value.parse().map_err(|error| {
                        format!("config key 'port' value {value:?} is not a port number: {error}")
                    })?
                }
                "keep_alive_timeout_ms" => {
                    config.keep_alive_timeout_ms = value.parse().map_err(|error| {
                        format!(
                            "config key 'keep_alive_timeout_ms' value {value:?} is not an \
                             unsigned integer: {error}"
                        )
                    })?;
                }
                "advertised_ip" => config.advertised_ip = crate::state::unquote(&value),
                "max_datagrams_per_tick" => {
                    config.max_datagrams_per_tick = value.parse().map_err(|error| {
                        format!(
                            "config key 'max_datagrams_per_tick' value {value:?} is not an \
                             unsigned integer: {error}"
                        )
                    })?;
                }
                _ => unknown.push(key),
            }
        }
        if !unknown.is_empty() {
            tracing::warn!(?unknown, "ignoring unknown keys in the voice config file");
        }
        Ok(config)
    }

    /// Renders the config back to flat TOML, for examples and tests.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut out = String::from("# Plasmo Voice server configuration\n");
        out.push_str(&format!(
            "port = {}\nkeep_alive_timeout_ms = {}\nadvertised_ip = {}\n\
             max_datagrams_per_tick = {}\n",
            self.port,
            self.keep_alive_timeout_ms,
            render_string(&self.advertised_ip, "\"0.0.0.0\""),
            self.max_datagrams_per_tick,
        ));
        out
    }
}

/// Renders a string for flat TOML: quoted, with the default spelled out plainly.
fn render_string(value: &str, default: &str) -> String {
    if value == "0.0.0.0" {
        return default.to_string();
    }
    format!("{value:?}")
}

#[cfg(test)]
mod plugin_config_tests {
    use super::*;

    #[test]
    fn the_default_config_keeps_the_historical_behaviour() {
        let config = PluginConfig::default();
        assert_eq!(config.port, 0, "port 0 means 'trust state.toml'");
        assert_eq!(config.keep_alive_timeout_ms, 15_000);
        assert_eq!(config.advertised_ip, "0.0.0.0");
        assert_eq!(config.max_datagrams_per_tick, 256);
    }

    #[test]
    fn a_minimal_config_file_overrides_only_what_it_sets() {
        let config = PluginConfig::from_toml("# comment\nkeep_alive_timeout_ms = 5000\n")
            .expect("known keys parse");
        assert_eq!(config.port, 0, "unset keys keep defaults");
        assert_eq!(config.keep_alive_timeout_ms, 5000);
        assert_eq!(config.advertised_ip, "0.0.0.0");
        assert_eq!(config.max_datagrams_per_tick, 256);
    }

    #[test]
    fn a_full_config_file_parses_every_key() {
        let config = PluginConfig::from_toml(
            "port = 24424\nkeep_alive_timeout_ms = 30000\nadvertised_ip = \"mc.example.com\"\n\
             max_datagrams_per_tick = 512\n",
        )
        .expect("all keys parse");
        assert_eq!(config.port, 24424);
        assert_eq!(config.keep_alive_timeout_ms, 30_000);
        assert_eq!(config.advertised_ip, "mc.example.com");
        assert_eq!(config.max_datagrams_per_tick, 512);
    }

    #[test]
    fn an_unknown_key_is_ignored_not_fatal() {
        let config = PluginConfig::from_toml("future_key = \"soon\"\nport = 12\n")
            .expect("unknown keys are skipped");
        assert_eq!(config.port, 12);
    }

    #[test]
    fn a_bad_value_for_a_known_key_is_reported() {
        let error = PluginConfig::from_toml("keep_alive_timeout_ms = \"not a number\"\n")
            .expect_err("a non-numeric known key fails");
        assert!(error.contains("keep_alive_timeout_ms"), "{error}");
    }

    #[test]
    fn to_toml_round_trips() {
        let config = PluginConfig::from_toml(
            "port = 24424\nadvertised_ip = \"mc.example.com\"\nmax_datagrams_per_tick = 100\n",
        )
        .expect("parses");
        let rendered = config.to_toml();
        let parsed = PluginConfig::from_toml(&rendered).expect("rendered config parses");
        // `to_toml` writes the default keep-alive back out as its default literal;
        // everything else must round-trip exactly.
        assert_eq!(parsed.port, config.port);
        assert_eq!(parsed.advertised_ip, config.advertised_ip);
        assert_eq!(parsed.max_datagrams_per_tick, config.max_datagrams_per_tick);
        assert_eq!(parsed.keep_alive_timeout_ms, 15_000);
        assert_eq!(parsed, config);
    }

    #[test]
    fn the_default_template_parses_to_the_default_config() {
        let parsed = PluginConfig::from_toml(DEFAULT_CONFIG_TOML).expect("the template parses");
        assert_eq!(parsed, PluginConfig::default());
    }

    #[test]
    fn the_template_is_written_only_when_the_file_is_missing() {
        let dir = temp_folder();
        let path = format!("{dir}/{CONFIG_FILE}");

        let created = PluginConfig::create_default_if_missing(&dir).expect("first write works");
        assert!(created, "a missing file is created");
        assert!(
            std::path::Path::new(&path).exists(),
            "the template lands on disk"
        );

        // An operator's own file must never be overwritten.
        std::fs::write(&path, "port = 7777\n").expect("operator file writes");
        let created_again = PluginConfig::create_default_if_missing(&dir).expect("second call ok");
        assert!(!created_again, "an existing file is left alone");
        let parsed = PluginConfig::load(&dir);
        assert_eq!(parsed.port, 7777, "the operator's file is what is read");

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// A uniquely-named scratch folder under the system temp dir, so parallel tests
/// never collide.
#[cfg(test)]
fn temp_folder() -> String {
    let unique = format!(
        "plasmo-voice-config-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("scratch dir creates");
    dir.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proximity_activation_matches_upstreams_defaults() {
        let activation = proximity_activation();
        assert_eq!(activation.name, "proximity");
        assert_eq!(activation.distances, vec![8, 16, 32]);
        assert_eq!(activation.default_distance, 16);
        assert!(activation.proximity);
        assert!(activation.transitive);
        assert!(!activation.stereo_supported);
        assert!(activation.encoder_info.is_none());
        // The id is derived from the name, and must be the MD5 name-UUID of
        // "proximity_activation" — a stable value, so pin it.
        assert_eq!(
            activation.id,
            VoiceActivation::generate_id("proximity"),
            "the activation id is re-derived from the name"
        );
        assert_eq!(
            activation.id.to_string(),
            "4aec07ba-d109-3345-9a0a-92022a116cd0",
            "the core already pins this value for `proximity`; keep them agreeing"
        );
    }

    #[test]
    fn the_proximity_line_matches_upstreams_defaults() {
        let line = proximity_source_line();
        assert_eq!(line.name, "proximity");
        assert_eq!(line.default_volume, 1.0);
        assert_eq!(line.weight, 1);
        assert!(
            line.players.is_none(),
            "the proximity line has no player set"
        );
    }

    #[test]
    fn the_capture_info_carries_a_mandatory_opus_encoder() {
        let capture = capture_info();
        assert_eq!(capture.sample_rate, 48_000);
        assert_eq!(capture.mtu_size, 1024);
        let encoder = capture
            .encoder_info
            .expect("a client throws when the encoder info is missing");
        assert_eq!(encoder.name, "opus");
        assert_eq!(
            encoder.params,
            vec![
                ("mode".to_string(), "VOIP".to_string()),
                ("bitrate".to_string(), "-1000".to_string())
            ]
        );
    }

    #[test]
    fn a_config_packet_always_carries_the_fields_a_client_dereferences() {
        let id = Uuid::from_u128(0x1234_5678);
        let packet = ServerConfig::new(id).config_packet(None);
        assert_eq!(packet.server_id, id);
        assert!(
            packet.encryption.is_none(),
            "no key offered: plaintext audio"
        );
        assert_eq!(packet.activations.len(), 1);
        assert_eq!(packet.source_lines.len(), 1);
        assert!(packet.player_icon_config.is_some());
        let icon = packet
            .player_icon_config
            .expect("never optional on the wire");
        assert!(icon.icon_visibility.is_empty());
        assert_eq!(icon.icon_offset, Pos3d::zero());
    }

    #[test]
    fn the_config_packet_round_trips_through_the_wire_codec() {
        use plasmo_voice_core::{PacketDirection, TcpCodec, TcpPacket};

        let codec = TcpCodec::new();
        let packet = ServerConfig::new(Uuid::from_u128(0xfeed)).config_packet(None);
        let bytes = codec
            .encode(&TcpPacket::Config(packet.clone()))
            .expect("encode");
        let decoded = codec
            .decode(&bytes, PacketDirection::Client)
            .expect("decode ok")
            .expect("a registered id");
        assert_eq!(decoded, TcpPacket::Config(packet));
    }

    #[test]
    fn versions_parse_and_gate_exactly_like_upstream() {
        assert_eq!(
            Version::parse("2.1.7"),
            Some(Version {
                major: 2,
                minor: 1,
                patch: 7
            })
        );
        assert_eq!(
            Version::parse("2.1"),
            Some(Version {
                major: 2,
                minor: 1,
                patch: 0
            })
        );
        assert_eq!(
            Version::parse("2"),
            Some(Version {
                major: 2,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(
            Version::parse("2.1.7-beta.1"),
            Some(Version {
                major: 2,
                minor: 1,
                patch: 7
            })
        );
        assert_eq!(Version::parse("two"), None);
        assert_eq!(Version::parse("2.1.7.9"), None);

        assert!(client_version_is_supported("2.1.7").is_ok());
        assert!(client_version_is_supported("2.0.0").is_ok());
        // Major mismatch: upstream refuses and only suggests a version.
        assert!(client_version_is_supported("3.0.0").is_err());
        // Below the 2.0.0 minimum.
        assert!(client_version_is_supported("1.9.9").is_err());
        assert!(client_version_is_supported("garbage").is_err());
    }

    #[test]
    fn distance_clamping_matches_check_distance() {
        let activation = proximity_activation();
        assert_eq!(calculate_allowed_distance(&activation, 16), 16);
        assert_eq!(calculate_allowed_distance(&activation, 8), 8);
        assert_eq!(calculate_allowed_distance(&activation, 32), 32);
        // 20 is not one of [8, 16, 32], so it collapses to the default.
        assert_eq!(calculate_allowed_distance(&activation, 20), 16);
        assert_eq!(calculate_allowed_distance(&activation, 0), 16);

        let empty = VoiceActivation {
            distances: Vec::new(),
            ..proximity_activation()
        };
        assert!(check_distance(&empty, 12345), "an empty list allows any");

        let dynamic = VoiceActivation {
            distances: vec![-1, 30],
            ..proximity_activation()
        };
        assert!(check_distance(&dynamic, 1));
        assert!(check_distance(&dynamic, 30));
        assert!(!check_distance(&dynamic, 0));
        assert!(!check_distance(&dynamic, 31));
    }
}
