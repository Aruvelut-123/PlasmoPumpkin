//! The server's `ConfigPacket` — everything a client needs before it can speak.
//!
//! Upstream builds this in `VoiceTcpServerConnectionManager.sendConfigInfo`
//! (`server/common/src/main/java/su/plo/voice/server/connection/VoiceTcpServerConnectionManager.java:97`)
//! out of `VoiceServerConfig` plus the activation and source-line registries. This plugin
//! has no TOML config yet, so it ships the upstream **defaults**, and every constant below
//! names the config key it stands in for so a future config file has somewhere to land.
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
//! * `encryption` — genuinely optional: `null` means "plaintext audio", which the client
//!   accepts. See [`ServerConfig::config_packet`] for the deviation that comes with it.

use plasmo_voice_core::data::{
    CaptureInfo, CodecInfo, PlayerIconConfig, Pos3d, VoiceActivation, VoiceSourceLine,
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

/// The client-scoped translations this server ships, as `LanguagePacket.language`.
///
/// The official server ships `languages/en_us.toml` and sends the client-scoped half of it
/// on every `LanguageRequestPacket` (`PlayerChannelHandler:203-213` →
/// `getClientLanguage(language)`). That file has exactly one `[client.*]` entry:
///
/// ```toml
/// [client.pv.activation]
/// proximity = "Proximity"
/// ```
///
/// It matters more than its size suggests. The mod's own `en_us.json` does **not** define
/// `pv.activation.proximity`, and the volume tab translates the source line's translation
/// key (`VolumeTabWidget:125`), so a server that sends an empty map makes the client
/// display the raw key `pv.activation.proximity` where "Proximity" belongs.
///
/// We ship no translations of our own, so every requested language gets the English
/// fallback — the same text upstream falls back to for a language it does not have.
#[must_use]
pub fn client_language() -> Vec<(String, String)> {
    vec![(PROXIMITY_TRANSLATION.to_string(), "Proximity".to_string())]
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
    /// ## Deviation: no encryption
    ///
    /// Upstream RSA-encrypts a 16-byte AES key with the public key the client sent in its
    /// `PlayerInfoPacket` (`VoiceTcpServerConnectionManager.java:110`) and lets a failure
    /// abort the config packet entirely. This plugin sends `encryption: None` instead,
    /// which the protocol and the client both support: `ModServerConnection` only installs
    /// a cipher when `getEncryption() != null`. The consequence is honest and worth
    /// stating plainly — **UDP audio is not encrypted**, and the client sends plaintext
    /// Opus frames. Doing it properly means an RSA implementation in the guest and a
    /// persisted AES key; until then, plaintext is the documented behaviour rather than a
    /// silently different key.
    #[must_use]
    pub fn config_packet(&self) -> ConfigPacket {
        ConfigPacket {
            server_id: self.server_id,
            capture_info: capture_info(),
            encryption: None,
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
        let packet = ServerConfig::new(id).config_packet();
        assert_eq!(packet.server_id, id);
        assert!(packet.encryption.is_none(), "documented: plaintext audio");
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
        let packet = ServerConfig::new(Uuid::from_u128(0xfeed)).config_packet();
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
