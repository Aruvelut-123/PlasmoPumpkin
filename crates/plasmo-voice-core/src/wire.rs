//! Packet codecs for the Plasmo Voice protocol.
//!
//! - [`TcpCodec`]: 26 packets over TCP, framed as `writeByte(type) + body`.
//! - [`UdpCodec`]: 5 packets over UDP, framed as
//!   `magic (u32) + type (i8) + secret (UUID) + timestamp (i64) + body`.
//!
//! Packet ids and directions match `packets/PacketTcpCodec.java` and
//! `packets/udp/PacketUdpCodec.java` exactly. Unknown ids (or ids that are
//! registered for another direction) decode to `None`, mirroring the upstream
//! registry which returns an empty `Optional` in that case.

use uuid::Uuid;

use crate::data::{
    CaptureInfo, EncryptionInfo, McGameProfile, PlayerIconConfig, Pos3d, SelfSourceInfo,
    SourceInfo, VoiceActivation, VoicePlayerInfo, VoiceSourceLine,
};
use crate::error::{Result, VoiceError};
use crate::util::{WireReader, WireWriter};

// ---------------------------------------------------------------------------
// Directions
// ---------------------------------------------------------------------------

/// In which direction a packet id is valid.
///
/// Mirrors `PacketDirection`: `CLIENT` means the *client* receives it,
/// `SERVER` means the *client* sends it, `ANY` both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketDirection {
    /// Server -> client (client receives).
    Client,
    /// Client -> server (client sends).
    Server,
    /// Either direction.
    Any,
}

impl PacketDirection {
    /// `PacketDirection.accepts`: matches if either side is `Any`.
    pub fn accepts(self, direction: PacketDirection) -> bool {
        self == PacketDirection::Any
            || direction == PacketDirection::Any
            || self == direction
    }
}

// ---------------------------------------------------------------------------
// Optional Pos3d helpers (PacketUtil.readNullable/writeNullable + Pos3d)
// ---------------------------------------------------------------------------

fn write_optional_pos3d(out: &mut WireWriter, pos: Option<&Pos3d>) {
    out.write_bool(pos.is_some());
    if let Some(p) = pos {
        p.serialize(out);
    }
}

fn read_optional_pos3d(input: &mut WireReader) -> Result<Option<Pos3d>> {
    if input.read_bool()? {
        Ok(Some(Pos3d::deserialize(input)?))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// PacketRegistry
// ---------------------------------------------------------------------------

type TcpFactory = fn(&mut WireReader) -> Result<TcpPacket>;
type UdpFactory = fn(&mut WireReader) -> Result<UdpPacket>;

/// Backing registry mapping packet ids to decode factories, mirroring
/// `su.plo.voice.proto.packets.PacketRegistry` (three maps folded into two
/// lists since ids are unique per codec).
#[derive(Debug, Clone)]
pub struct PacketRegistry {
    tcp: Vec<(u32, PacketDirection, TcpFactory)>,
    udp: Vec<(u32, PacketDirection, UdpFactory)>,
}

impl PacketRegistry {
    /// The default registry with every id both codecs know about.
    pub fn new() -> Self {
        Self {
            tcp: tcp_entries(),
            udp: udp_entries(),
        }
    }

    /// `PacketRegistry.byType(TcpPacket)`: the decode factory for a TCP id in
    /// the given direction, if registered (mirrors the upstream Optional).
    pub fn tcp_by_type(&self, id: u32, direction: PacketDirection) -> Option<TcpFactory> {
        self.tcp
            .iter()
            .find(|(i, dir, _)| *i == id && dir.accepts(direction))
            .map(|(_, _, f)| *f)
    }

    /// `PacketRegistry.byType(UdpPacket)`: the decode factory for a UDP id in
    /// the given direction, if registered (mirrors the upstream Optional).
    pub fn udp_by_type(&self, id: u32, direction: PacketDirection) -> Option<UdpFactory> {
        self.udp
            .iter()
            .find(|(i, dir, _)| *i == id && dir.accepts(direction))
            .map(|(_, _, f)| *f)
    }

    /// `PacketRegistry.getType(TcpPacket)`: the registered id, or `-1` if the
    /// packet class is unknown (never happens for the closed enum).
    pub fn get_tcp_id(&self, packet: &TcpPacket) -> i32 {
        packet.id() as i32
    }

    /// `PacketRegistry.getType(UdpPacket)` including the `0x100` custom id.
    pub fn get_udp_id(&self, packet: &UdpPacket) -> i32 {
        match packet {
            UdpPacket::Custom(_) => 0x100,
            _ => packet.id() as i32,
        }
    }

    /// Registered direction of a TCP id, if any.
    pub fn tcp_direction(&self, id: u32) -> Option<PacketDirection> {
        self.tcp.iter().find(|(i, _, _)| *i == id).map(|(_, d, _)| *d)
    }

    /// Registered direction of a UDP id, if any.
    pub fn udp_direction(&self, id: u32) -> Option<PacketDirection> {
        self.udp.iter().find(|(i, _, _)| *i == id).map(|(_, d, _)| *d)
    }
}

impl Default for PacketRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// TCP packets
// ---------------------------------------------------------------------------

/// `ConnectionPacket` (0x01, CLIENT): secret UUID, server ip, server port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPacket {
    pub secret: Uuid,
    pub ip: String,
    pub port: i32,
}

/// `PlayerInfoRequestPacket` (0x02, CLIENT): empty body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlayerInfoRequestPacket;

/// `ConfigPacket` (0x03, CLIENT): full server config, extends
/// `ConfigPlayerInfoPacket`. The trailing `player_icon_config` is optional:
/// pre-2.1.7 servers do not send it, so a failed decode yields `None`
/// (upstream catches the exception and keeps the field null).
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPacket {
    pub server_id: Uuid,
    pub capture_info: CaptureInfo,
    pub encryption: Option<EncryptionInfo>,
    pub source_lines: Vec<VoiceSourceLine>,
    pub activations: Vec<VoiceActivation>,
    pub permissions: Vec<(String, bool)>,
    pub player_icon_config: Option<PlayerIconConfig>,
}

/// `ConfigPlayerInfoPacket` (0x04, CLIENT): permission key/boolean pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPlayerInfoPacket {
    pub permissions: Vec<(String, bool)>,
}

/// `LanguageRequestPacket` (0x05, SERVER): language tag like `en_us`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageRequestPacket {
    pub language: String,
}

/// `LanguagePacket` (0x06, CLIENT): language name and translation map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguagePacket {
    pub language_name: String,
    pub language: Vec<(String, String)>,
}

/// `PlayerListPacket` (0x07, CLIENT): all known voice players.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerListPacket {
    pub players: Vec<VoicePlayerInfo>,
}

/// `PlayerInfoUpdatePacket` (0x08, CLIENT): one player update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerInfoUpdatePacket {
    pub player_info: VoicePlayerInfo,
}

/// `PlayerDisconnectPacket` (0x09, CLIENT): player left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerDisconnectPacket {
    pub player_id: Uuid,
}

/// `PlayerInfoPacket` (0x0A, SERVER): extends `PlayerStatePacket`, adds
/// versions and the RSA public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerInfoPacket {
    pub voice_disabled: bool,
    pub microphone_muted: bool,
    pub minecraft_version: String,
    pub version: String,
    pub public_key: Vec<u8>,
}

/// `PlayerStatePacket` (0x0B, SERVER): voice disabled + microphone muted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerStatePacket {
    pub voice_disabled: bool,
    pub microphone_muted: bool,
}

/// `PlayerAudioEndPacket` (0x0C, SERVER): the client must stop the audio
/// stream for `activation_id` after `sequence_number`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerAudioEndPacket {
    pub sequence_number: i64,
    pub activation_id: Uuid,
    pub distance: i16,
}

/// `PlayerActivationDistancesPacket` (0x0D, SERVER): per-activation distances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerActivationDistancesPacket {
    pub distance_by_activation_id: Vec<(Uuid, i32)>,
}

/// `DistanceVisualizePacket` (0x0E, CLIENT): render a distance circle.
#[derive(Debug, Clone, PartialEq)]
pub struct DistanceVisualizePacket {
    pub radius: i32,
    pub hex_color: i32,
    pub position: Option<Pos3d>,
}

/// `SourceInfoRequestPacket` (0x0F, SERVER): ask for a source's info.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfoRequestPacket {
    pub source_id: Uuid,
}

/// `SourceInfoPacket` (0x10, CLIENT): a source info (type-tagged).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfoPacket {
    pub source_info: SourceInfo,
}

/// `SelfSourceInfoPacket` (0x11, CLIENT): self source info.
#[derive(Debug, Clone, PartialEq)]
pub struct SelfSourceInfoPacket {
    pub source_info: SelfSourceInfo,
}

/// `SourceAudioEndPacket` (0x12, CLIENT): stop the source audio stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAudioEndPacket {
    pub source_id: Uuid,
    pub sequence_number: i64,
}

/// `ActivationRegisterPacket` (0x13, CLIENT): register an activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationRegisterPacket {
    pub activation: VoiceActivation,
}

/// `ActivationUnregisterPacket` (0x14, CLIENT): unregister an activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationUnregisterPacket {
    pub activation_id: Uuid,
}

/// `SourceLineRegisterPacket` (0x15, CLIENT): register a source line.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceLineRegisterPacket {
    pub source_line: VoiceSourceLine,
}

/// `SourceLineUnregisterPacket` (0x16, CLIENT): unregister a source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLineUnregisterPacket {
    pub line_id: Uuid,
}

/// `SourceLinePlayerAddPacket` (0x17, CLIENT): add a player to a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLinePlayerAddPacket {
    pub line_id: Uuid,
    pub player: McGameProfile,
}

/// `SourceLinePlayerRemovePacket` (0x18, CLIENT): remove a player from a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLinePlayerRemovePacket {
    pub line_id: Uuid,
    pub player_id: Uuid,
}

/// `SourceLinePlayersListPacket` (0x19, CLIENT): full player list of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLinePlayersListPacket {
    pub line_id: Uuid,
    pub players: Vec<McGameProfile>,
}

/// `AnimatedActionBarPacket` (0x1A, CLIENT): animated action bar JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimatedActionBarPacket {
    pub json_component: String,
}

/// Any decodable TCP packet; the variant matches the registered class.
#[derive(Debug, Clone, PartialEq)]
pub enum TcpPacket {
    Connection(ConnectionPacket),
    PlayerInfoRequest(PlayerInfoRequestPacket),
    Config(ConfigPacket),
    ConfigPlayerInfo(ConfigPlayerInfoPacket),
    LanguageRequest(LanguageRequestPacket),
    Language(LanguagePacket),
    PlayerList(PlayerListPacket),
    PlayerInfoUpdate(PlayerInfoUpdatePacket),
    PlayerDisconnect(PlayerDisconnectPacket),
    PlayerInfo(PlayerInfoPacket),
    PlayerState(PlayerStatePacket),
    PlayerAudioEnd(PlayerAudioEndPacket),
    PlayerActivationDistances(PlayerActivationDistancesPacket),
    DistanceVisualize(DistanceVisualizePacket),
    SourceInfoRequest(SourceInfoRequestPacket),
    SourceInfo(SourceInfoPacket),
    SelfSourceInfo(SelfSourceInfoPacket),
    SourceAudioEnd(SourceAudioEndPacket),
    ActivationRegister(ActivationRegisterPacket),
    ActivationUnregister(ActivationUnregisterPacket),
    SourceLineRegister(SourceLineRegisterPacket),
    SourceLineUnregister(SourceLineUnregisterPacket),
    SourceLinePlayerAdd(SourceLinePlayerAddPacket),
    SourceLinePlayerRemove(SourceLinePlayerRemovePacket),
    SourceLinePlayersList(SourceLinePlayersListPacket),
    AnimatedActionBar(AnimatedActionBarPacket),
}

impl TcpPacket {
    /// Registered id in `[1, 26]`.
    pub fn id(&self) -> u32 {
        match self {
            TcpPacket::Connection(_) => 0x01,
            TcpPacket::PlayerInfoRequest(_) => 0x02,
            TcpPacket::Config(_) => 0x03,
            TcpPacket::ConfigPlayerInfo(_) => 0x04,
            TcpPacket::LanguageRequest(_) => 0x05,
            TcpPacket::Language(_) => 0x06,
            TcpPacket::PlayerList(_) => 0x07,
            TcpPacket::PlayerInfoUpdate(_) => 0x08,
            TcpPacket::PlayerDisconnect(_) => 0x09,
            TcpPacket::PlayerInfo(_) => 0x0A,
            TcpPacket::PlayerState(_) => 0x0B,
            TcpPacket::PlayerAudioEnd(_) => 0x0C,
            TcpPacket::PlayerActivationDistances(_) => 0x0D,
            TcpPacket::DistanceVisualize(_) => 0x0E,
            TcpPacket::SourceInfoRequest(_) => 0x0F,
            TcpPacket::SourceInfo(_) => 0x10,
            TcpPacket::SelfSourceInfo(_) => 0x11,
            TcpPacket::SourceAudioEnd(_) => 0x12,
            TcpPacket::ActivationRegister(_) => 0x13,
            TcpPacket::ActivationUnregister(_) => 0x14,
            TcpPacket::SourceLineRegister(_) => 0x15,
            TcpPacket::SourceLineUnregister(_) => 0x16,
            TcpPacket::SourceLinePlayerAdd(_) => 0x17,
            TcpPacket::SourceLinePlayerRemove(_) => 0x18,
            TcpPacket::SourceLinePlayersList(_) => 0x19,
            TcpPacket::AnimatedActionBar(_) => 0x1A,
        }
    }

    /// The direction this packet id is registered for.
    pub fn direction(&self) -> PacketDirection {
        match self {
            TcpPacket::LanguageRequest(_) | TcpPacket::PlayerInfo(_) | TcpPacket::PlayerState(_)
            | TcpPacket::PlayerAudioEnd(_) | TcpPacket::PlayerActivationDistances(_)
            | TcpPacket::SourceInfoRequest(_) => PacketDirection::Server,
            _ => PacketDirection::Client,
        }
    }

    /// Encodes as `writeByte(id) + body`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = WireWriter::new();
        out.write_i8(self.id() as i8);
        self.write_body(&mut out)?;
        Ok(out.into_inner())
    }

    /// Writes only the packet body (without the id byte).
    pub fn write_body(&self, out: &mut WireWriter) -> Result<()> {
        match self {
            TcpPacket::Connection(p) => {
                out.write_uuid(p.secret);
                out.write_utf(&p.ip)?;
                out.write_i32(p.port);
            }
            TcpPacket::PlayerInfoRequest(_) => {}
            TcpPacket::Config(p) => {
                out.write_uuid(p.server_id);
                p.capture_info.serialize(out)?;
                out.write_bool(p.encryption.is_some());
                if let Some(e) = &p.encryption {
                    e.serialize(out)?;
                }
                out.write_i32(p.source_lines.len() as i32);
                for line in &p.source_lines {
                    line.serialize(out)?;
                }
                out.write_i32(p.activations.len() as i32);
                for act in &p.activations {
                    act.serialize(out)?;
                }
                out.write_i32(p.permissions.len() as i32);
                for (k, v) in &p.permissions {
                    out.write_utf(k)?;
                    out.write_bool(*v);
                }
                if let Some(cfg) = &p.player_icon_config {
                    cfg.serialize(out)?;
                }
            }
            TcpPacket::ConfigPlayerInfo(p) => {
                out.write_i32(p.permissions.len() as i32);
                for (k, v) in &p.permissions {
                    out.write_utf(k)?;
                    out.write_bool(*v);
                }
            }
            TcpPacket::LanguageRequest(p) => {
                out.write_utf(&p.language)?;
            }
            TcpPacket::Language(p) => {
                out.write_utf(&p.language_name)?;
                if p.language.len() as i64 > i16::MAX as i64 {
                    return Err(VoiceError::InvalidState("language map too large"));
                }
                out.write_i32(p.language.len() as i32);
                for (k, v) in &p.language {
                    out.write_utf(k)?;
                    out.write_utf(v)?;
                }
            }
            TcpPacket::PlayerList(p) => {
                out.write_i32(p.players.len() as i32);
                for player in &p.players {
                    player.serialize(out)?;
                }
            }
            TcpPacket::PlayerInfoUpdate(p) => {
                p.player_info.serialize(out)?;
            }
            TcpPacket::PlayerDisconnect(p) => {
                out.write_uuid(p.player_id);
            }
            TcpPacket::PlayerInfo(p) => {
                out.write_bool(p.voice_disabled);
                out.write_bool(p.microphone_muted);
                out.write_utf(&p.minecraft_version)?;
                out.write_utf(&p.version)?;
                out.write_i32(p.public_key.len() as i32);
                out.write_bytes(&p.public_key);
            }
            TcpPacket::PlayerState(p) => {
                out.write_bool(p.voice_disabled);
                out.write_bool(p.microphone_muted);
            }
            TcpPacket::PlayerAudioEnd(p) => {
                out.write_i64(p.sequence_number);
                out.write_uuid(p.activation_id);
                out.write_i16(p.distance);
            }
            TcpPacket::PlayerActivationDistances(p) => {
                out.write_i32(p.distance_by_activation_id.len() as i32);
                for (id, d) in &p.distance_by_activation_id {
                    out.write_uuid(*id);
                    out.write_i32(*d);
                }
            }
            TcpPacket::DistanceVisualize(p) => {
                out.write_i32(p.radius);
                out.write_i32(p.hex_color);
                write_optional_pos3d(out, p.position.as_ref());
            }
            TcpPacket::SourceInfoRequest(p) => {
                out.write_uuid(p.source_id);
            }
            TcpPacket::SourceInfo(p) => {
                p.source_info.serialize(out)?;
            }
            TcpPacket::SelfSourceInfo(p) => {
                p.source_info.serialize(out)?;
            }
            TcpPacket::SourceAudioEnd(p) => {
                out.write_uuid(p.source_id);
                out.write_i64(p.sequence_number);
            }
            TcpPacket::ActivationRegister(p) => {
                p.activation.serialize(out)?;
            }
            TcpPacket::ActivationUnregister(p) => {
                out.write_uuid(p.activation_id);
            }
            TcpPacket::SourceLineRegister(p) => {
                p.source_line.serialize(out)?;
            }
            TcpPacket::SourceLineUnregister(p) => {
                out.write_uuid(p.line_id);
            }
            TcpPacket::SourceLinePlayerAdd(p) => {
                out.write_uuid(p.line_id);
                p.player.serialize(out)?;
            }
            TcpPacket::SourceLinePlayerRemove(p) => {
                out.write_uuid(p.line_id);
                out.write_uuid(p.player_id);
            }
            TcpPacket::SourceLinePlayersList(p) => {
                out.write_uuid(p.line_id);
                out.write_i32(p.players.len() as i32);
                for player in &p.players {
                    player.serialize(out)?;
                }
            }
            TcpPacket::AnimatedActionBar(p) => {
                out.write_utf(&p.json_component)?;
            }
        }
        Ok(())
    }

    /// Reads a full packet from `data` (id byte + body).
    pub fn decode(data: &[u8], direction: PacketDirection) -> Result<Option<TcpPacket>> {
        let mut input = WireReader::new(data);
        let id = input.read_i8()? as u8 as u32;
        Self::read_body(id, &mut input, direction)
    }

    /// Reads the body for `id` from `input`; unknown ids or wrong directions
    /// yield `None`.
    pub fn read_body(
        id: u32,
        input: &mut WireReader,
        direction: PacketDirection,
    ) -> Result<Option<TcpPacket>> {
        let factory = match tcp_entries()
            .into_iter()
            .find(|(i, dir, _)| *i == id && dir.accepts(direction))
        {
            Some((_, _, f)) => f,
            None => return Ok(None),
        };
        factory(input).map(Some)
    }
}

fn tcp_entries() -> Vec<(u32, PacketDirection, TcpFactory)> {
    use PacketDirection::*;
    vec![
        (0x01, Client, |r| Ok(TcpPacket::Connection(read_connection(r)?))),
        (0x02, Client, |r| Ok(TcpPacket::PlayerInfoRequest(read_player_info_request(r)?))),
        (0x03, Client, |r| Ok(TcpPacket::Config(read_config(r)?))),
        (0x04, Client, |r| Ok(TcpPacket::ConfigPlayerInfo(read_config_player_info(r)?))),
        (0x05, Server, |r| Ok(TcpPacket::LanguageRequest(read_language_request(r)?))),
        (0x06, Client, |r| Ok(TcpPacket::Language(read_language(r)?))),
        (0x07, Client, |r| Ok(TcpPacket::PlayerList(read_player_list(r)?))),
        (0x08, Client, |r| Ok(TcpPacket::PlayerInfoUpdate(read_player_info_update(r)?))),
        (0x09, Client, |r| Ok(TcpPacket::PlayerDisconnect(read_player_disconnect(r)?))),
        (0x0A, Server, |r| Ok(TcpPacket::PlayerInfo(read_player_info(r)?))),
        (0x0B, Server, |r| Ok(TcpPacket::PlayerState(read_player_state(r)?))),
        (0x0C, Server, |r| Ok(TcpPacket::PlayerAudioEnd(read_player_audio_end(r)?))),
        (0x0D, Server, |r| Ok(TcpPacket::PlayerActivationDistances(
            read_player_activation_distances(r)?,
        ))),
        (0x0E, Client, |r| Ok(TcpPacket::DistanceVisualize(read_distance_visualize(r)?))),
        (0x0F, Server, |r| Ok(TcpPacket::SourceInfoRequest(read_source_info_request(r)?))),
        (0x10, Client, |r| Ok(TcpPacket::SourceInfo(read_source_info(r)?))),
        (0x11, Client, |r| Ok(TcpPacket::SelfSourceInfo(read_self_source_info(r)?))),
        (0x12, Client, |r| Ok(TcpPacket::SourceAudioEnd(read_source_audio_end(r)?))),
        (0x13, Client, |r| Ok(TcpPacket::ActivationRegister(read_activation_register(r)?))),
        (0x14, Client, |r| Ok(TcpPacket::ActivationUnregister(
            read_activation_unregister(r)?,
        ))),
        (0x15, Client, |r| Ok(TcpPacket::SourceLineRegister(read_source_line_register(r)?))),
        (0x16, Client, |r| Ok(TcpPacket::SourceLineUnregister(
            read_source_line_unregister(r)?,
        ))),
        (0x17, Client, |r| Ok(TcpPacket::SourceLinePlayerAdd(
            read_source_line_player_add(r)?,
        ))),
        (0x18, Client, |r| Ok(TcpPacket::SourceLinePlayerRemove(
            read_source_line_player_remove(r)?,
        ))),
        (0x19, Client, |r| Ok(TcpPacket::SourceLinePlayersList(
            read_source_line_players_list(r)?,
        ))),
        (0x1A, Client, |r| Ok(TcpPacket::AnimatedActionBar(
            read_animated_action_bar(r)?,
        ))),
    ]
}

// -- per-packet readers ----------------------------------------------------

fn read_connection(r: &mut WireReader) -> Result<ConnectionPacket> {
    Ok(ConnectionPacket {
        secret: r.read_uuid()?,
        ip: r.read_utf()?,
        port: r.read_i32()?,
    })
}

fn read_player_info_request(_r: &mut WireReader) -> Result<PlayerInfoRequestPacket> {
    Ok(PlayerInfoRequestPacket)
}

fn read_config(r: &mut WireReader) -> Result<ConfigPacket> {
    let server_id = r.read_uuid()?;
    let capture_info = CaptureInfo::deserialize(r)?;
    let encryption = if r.read_bool()? {
        Some(EncryptionInfo::deserialize(r)?)
    } else {
        None
    };
    let source_lines = {
        let n = r.read_safe_int(0, i8::MAX as i32)? as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(VoiceSourceLine::deserialize(r)?);
        }
        v
    };
    let activations = {
        let n = r.read_safe_int(0, i8::MAX as i32)? as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(VoiceActivation::deserialize(r)?);
        }
        v
    };
    let permissions = {
        let n = r.read_safe_int(0, i8::MAX as i32)? as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push((r.read_utf()?, r.read_bool()?));
        }
        v
    };
    // since 2.1.7: servers on 2.1.6 omit this trailing section; treat a
    // failed read as "absent" and rewind so later use of the reader sees the
    // full packet unchanged (upstream swallows the exception).
    let start = r.pos();
    let player_icon_config = match PlayerIconConfig::deserialize(r) {
        Ok(cfg) => Some(cfg),
        Err(_) => {
            let _ = r.set_pos(start);
            None
        }
    };
    Ok(ConfigPacket {
        server_id,
        capture_info,
        encryption,
        source_lines,
        activations,
        permissions,
        player_icon_config,
    })
}

fn read_config_player_info(r: &mut WireReader) -> Result<ConfigPlayerInfoPacket> {
    let n = r.read_safe_int(0, i8::MAX as i32)? as usize;
    let mut permissions = Vec::with_capacity(n);
    for _ in 0..n {
        permissions.push((r.read_utf()?, r.read_bool()?));
    }
    Ok(ConfigPlayerInfoPacket { permissions })
}

fn read_language_request(r: &mut WireReader) -> Result<LanguageRequestPacket> {
    Ok(LanguageRequestPacket {
        language: r.read_safe_utf(32)?,
    })
}

fn read_language(r: &mut WireReader) -> Result<LanguagePacket> {
    let language_name = r.read_utf()?;
    let n = r.read_safe_int(0, i16::MAX as i32)? as usize;
    let mut language = Vec::with_capacity(n);
    for _ in 0..n {
        language.push((r.read_utf()?, r.read_utf()?));
    }
    Ok(LanguagePacket {
        language_name,
        language,
    })
}

fn read_player_list(r: &mut WireReader) -> Result<PlayerListPacket> {
    let n = r.read_safe_int(0, i16::MAX as i32)? as usize;
    let mut players = Vec::with_capacity(n);
    for _ in 0..n {
        players.push(VoicePlayerInfo::deserialize(r)?);
    }
    Ok(PlayerListPacket { players })
}

fn read_player_info_update(r: &mut WireReader) -> Result<PlayerInfoUpdatePacket> {
    Ok(PlayerInfoUpdatePacket {
        player_info: VoicePlayerInfo::deserialize(r)?,
    })
}

fn read_player_disconnect(r: &mut WireReader) -> Result<PlayerDisconnectPacket> {
    Ok(PlayerDisconnectPacket {
        player_id: r.read_uuid()?,
    })
}

fn read_player_info(r: &mut WireReader) -> Result<PlayerInfoPacket> {
    let voice_disabled = r.read_bool()?;
    let microphone_muted = r.read_bool()?;
    let minecraft_version = r.read_safe_utf(64)?;
    let version = r.read_safe_utf(64)?;
    let n = r.read_safe_int(1, 2048)? as usize;
    let public_key = r.read_bytes(n)?;
    Ok(PlayerInfoPacket {
        voice_disabled,
        microphone_muted,
        minecraft_version,
        version,
        public_key,
    })
}

fn read_player_state(r: &mut WireReader) -> Result<PlayerStatePacket> {
    Ok(PlayerStatePacket {
        voice_disabled: r.read_bool()?,
        microphone_muted: r.read_bool()?,
    })
}

fn read_player_audio_end(r: &mut WireReader) -> Result<PlayerAudioEndPacket> {
    Ok(PlayerAudioEndPacket {
        sequence_number: r.read_i64()?,
        activation_id: r.read_uuid()?,
        distance: r.read_i16()?,
    })
}

fn read_player_activation_distances(r: &mut WireReader) -> Result<PlayerActivationDistancesPacket> {
    let n = r.read_safe_int(0, i8::MAX as i32)? as usize;
    let mut distance_by_activation_id = Vec::with_capacity(n);
    for _ in 0..n {
        distance_by_activation_id.push((r.read_uuid()?, r.read_i32()?));
    }
    Ok(PlayerActivationDistancesPacket {
        distance_by_activation_id,
    })
}

fn read_distance_visualize(r: &mut WireReader) -> Result<DistanceVisualizePacket> {
    Ok(DistanceVisualizePacket {
        radius: r.read_i32()?,
        hex_color: r.read_i32()?,
        position: read_optional_pos3d(r)?,
    })
}

fn read_source_info_request(r: &mut WireReader) -> Result<SourceInfoRequestPacket> {
    Ok(SourceInfoRequestPacket {
        source_id: r.read_uuid()?,
    })
}

fn read_source_info(r: &mut WireReader) -> Result<SourceInfoPacket> {
    Ok(SourceInfoPacket {
        source_info: SourceInfo::deserialize(r)?,
    })
}

fn read_self_source_info(r: &mut WireReader) -> Result<SelfSourceInfoPacket> {
    Ok(SelfSourceInfoPacket {
        source_info: SelfSourceInfo::deserialize(r)?,
    })
}

fn read_source_audio_end(r: &mut WireReader) -> Result<SourceAudioEndPacket> {
    Ok(SourceAudioEndPacket {
        source_id: r.read_uuid()?,
        sequence_number: r.read_i64()?,
    })
}

fn read_activation_register(r: &mut WireReader) -> Result<ActivationRegisterPacket> {
    Ok(ActivationRegisterPacket {
        activation: VoiceActivation::deserialize(r)?,
    })
}

fn read_activation_unregister(r: &mut WireReader) -> Result<ActivationUnregisterPacket> {
    Ok(ActivationUnregisterPacket {
        activation_id: r.read_uuid()?,
    })
}

fn read_source_line_register(r: &mut WireReader) -> Result<SourceLineRegisterPacket> {
    Ok(SourceLineRegisterPacket {
        source_line: VoiceSourceLine::deserialize(r)?,
    })
}

fn read_source_line_unregister(r: &mut WireReader) -> Result<SourceLineUnregisterPacket> {
    Ok(SourceLineUnregisterPacket {
        line_id: r.read_uuid()?,
    })
}

fn read_source_line_player_add(r: &mut WireReader) -> Result<SourceLinePlayerAddPacket> {
    Ok(SourceLinePlayerAddPacket {
        line_id: r.read_uuid()?,
        player: McGameProfile::deserialize(r)?,
    })
}

fn read_source_line_player_remove(r: &mut WireReader) -> Result<SourceLinePlayerRemovePacket> {
    Ok(SourceLinePlayerRemovePacket {
        line_id: r.read_uuid()?,
        player_id: r.read_uuid()?,
    })
}

fn read_source_line_players_list(r: &mut WireReader) -> Result<SourceLinePlayersListPacket> {
    Ok(SourceLinePlayersListPacket {
        line_id: r.read_uuid()?,
        players: {
            let n = r.read_i32()?.max(0) as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(McGameProfile::deserialize(r)?);
            }
            v
        },
    })
}

fn read_animated_action_bar(r: &mut WireReader) -> Result<AnimatedActionBarPacket> {
    Ok(AnimatedActionBarPacket {
        json_component: r.read_utf()?,
    })
}

/// `PacketTcpCodec`: frames TCP packets as a single id byte plus the body.
#[derive(Debug, Clone, Default)]
pub struct TcpCodec {
    registry: PacketRegistry,
}

impl TcpCodec {
    pub fn new() -> Self {
        Self {
            registry: PacketRegistry::new(),
        }
    }

    pub fn registry(&self) -> &PacketRegistry {
        &self.registry
    }

    /// `PacketTcpCodec.encode`.
    pub fn encode(&self, packet: &TcpPacket) -> Result<Vec<u8>> {
        packet.encode()
    }

    /// `PacketTcpCodec.decode`: `None` for unknown ids or ids registered for a
    /// different direction, mirroring the upstream `Optional` semantics.
    pub fn decode(&self, data: &[u8], direction: PacketDirection) -> Result<Option<TcpPacket>> {
        TcpPacket::decode(data, direction)
    }

    /// Decodes only the body of a packet whose id is already known.
    pub fn decode_body(
        &self,
        id: u32,
        body: &[u8],
        direction: PacketDirection,
    ) -> Result<Option<TcpPacket>> {
        let mut input = WireReader::new(body);
        TcpPacket::read_body(id, &mut input, direction)
    }
}

// ---------------------------------------------------------------------------
// UDP packets
// ---------------------------------------------------------------------------

/// `PacketUdpCodec.MAGIC_NUMBER`: used to filter out packets not from PV.
pub const UDP_MAGIC: u32 = 0x4e90_04e9;

/// `PingPacket` (0x01, ANY): timestamp plus optional server endpoint.
///
/// The trailing ip/port is only written when both are present, and its decode
/// is wrapped in a try/catch upstream — reading it here is best-effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingPacket {
    pub time: i64,
    pub server_ip: Option<String>,
    pub server_port: Option<u16>,
}

impl PingPacket {
    /// Creates a ping with the current system time (like the no-arg Java
    /// constructor defaulting `time = System.currentTimeMillis()`).
    pub fn new(server_ip: Option<String>, server_port: u16) -> Self {
        Self {
            time: now_millis(),
            server_ip,
            server_port: (server_port > 0).then_some(server_port),
        }
    }
}

/// `PlayerAudioPacket` (0x02, SERVER): audio from a player activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerAudioPacket {
    pub sequence_number: i64,
    pub data: Vec<u8>,
    pub activation_id: Uuid,
    pub distance: i16,
    pub stereo: bool,
}

/// `SourceAudioPacket` (0x03, CLIENT): audio from a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAudioPacket {
    pub sequence_number: i64,
    pub data: Vec<u8>,
    pub source_id: Uuid,
    pub source_state: i8,
    pub distance: i16,
}

/// `SelfAudioInfoPacket` (0x04, CLIENT): info about one's own stream; data is
/// present only when the server changed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfAudioInfoPacket {
    pub source_id: Uuid,
    pub sequence_number: i64,
    pub data: Option<Vec<u8>>,
    pub distance: i16,
}

/// `CustomPacket` (0x100, ANY): addon-defined payload.
///
/// Id `0x100` truncates to `0x00` through `writeByte`, so this packet can
/// never actually be decoded from the wire (the registry knows no id `0x00`) —
/// exactly like upstream. Reading follows the upstream `read`, which **only**
/// consumes the addon id and leaves the payload untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomPacket {
    pub addon_id: String,
    pub payload: Option<Vec<u8>>,
}

/// Any decodable UDP packet body (the frame header is separate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpPacket {
    Ping(PingPacket),
    PlayerAudio(PlayerAudioPacket),
    SourceAudio(SourceAudioPacket),
    SelfAudioInfo(SelfAudioInfoPacket),
    Custom(CustomPacket),
}

impl UdpPacket {
    /// Registered id; `Custom` yields `0x100` which truncates to `0x00` in
    /// `writeByte`.
    pub fn id(&self) -> u32 {
        match self {
            UdpPacket::Ping(_) => 0x01,
            UdpPacket::PlayerAudio(_) => 0x02,
            UdpPacket::SourceAudio(_) => 0x03,
            UdpPacket::SelfAudioInfo(_) => 0x04,
            UdpPacket::Custom(_) => 0x100,
        }
    }

    /// The direction this packet id is registered for.
    pub fn direction(&self) -> PacketDirection {
        match self {
            UdpPacket::Ping(_) | UdpPacket::Custom(_) => PacketDirection::Any,
            UdpPacket::PlayerAudio(_) => PacketDirection::Server,
            UdpPacket::SourceAudio(_) | UdpPacket::SelfAudioInfo(_) => PacketDirection::Client,
        }
    }

    /// Writes only the body.
    pub fn write_body(&self, out: &mut WireWriter) -> Result<()> {
        match self {
            UdpPacket::Ping(p) => {
                out.write_i64(p.time);
                if let (Some(ip), Some(port)) = (&p.server_ip, p.server_port) {
                    if port > 0 {
                        out.write_utf(ip)?;
                        out.write_u16(port);
                    }
                }
            }
            UdpPacket::PlayerAudio(p) => {
                out.write_i64(p.sequence_number);
                out.write_i32(p.data.len() as i32);
                out.write_bytes(&p.data);
                out.write_uuid(p.activation_id);
                out.write_i16(p.distance);
                out.write_bool(p.stereo);
            }
            UdpPacket::SourceAudio(p) => {
                out.write_i64(p.sequence_number);
                out.write_i32(p.data.len() as i32);
                out.write_bytes(&p.data);
                out.write_uuid(p.source_id);
                out.write_i8(p.source_state);
                out.write_i16(p.distance);
            }
            UdpPacket::SelfAudioInfo(p) => {
                out.write_uuid(p.source_id);
                out.write_i64(p.sequence_number);
                out.write_bool(p.data.is_some());
                if let Some(data) = &p.data {
                    out.write_i32(data.len() as i32);
                    out.write_bytes(data);
                }
                out.write_i16(p.distance);
            }
            UdpPacket::Custom(p) => {
                out.write_utf(&p.addon_id)?;
                match &p.payload {
                    Some(payload) => {
                        out.write_i32(payload.len() as i32);
                        out.write_bytes(payload);
                    }
                    None => return Err(VoiceError::NullValue),
                }
            }
        }
        Ok(())
    }

    /// Reads a body after the 25-byte UDP frame header.
    pub fn read_body(id: u32, input: &mut WireReader) -> Result<UdpPacket> {
        match id {
            0x01 => Ok(UdpPacket::Ping(read_ping(input)?)),
            0x02 => Ok(UdpPacket::PlayerAudio(read_player_audio(input)?)),
            0x03 => Ok(UdpPacket::SourceAudio(read_source_audio(input)?)),
            0x04 => Ok(UdpPacket::SelfAudioInfo(read_self_audio_info(input)?)),
            // 0x100 never decodes: writeByte truncates it and the decoded id
            // is 0x00, which is not registered (upstream behaviour).
            _ => Err(VoiceError::UnknownPacketId(id)),
        }
    }
}

fn udp_entries() -> Vec<(u32, PacketDirection, UdpFactory)> {
    use PacketDirection::*;
    vec![
        (0x01, Any, |r| Ok(UdpPacket::Ping(read_ping(r)?))),
        (0x02, Server, |r| Ok(UdpPacket::PlayerAudio(read_player_audio(r)?))),
        (0x03, Client, |r| Ok(UdpPacket::SourceAudio(read_source_audio(r)?))),
        (0x04, Client, |r| Ok(UdpPacket::SelfAudioInfo(read_self_audio_info(r)?))),
        (0x100, Any, |r| Ok(UdpPacket::Custom(read_custom(r)?))),
    ]
}

fn read_ping(r: &mut WireReader) -> Result<PingPacket> {
    let time = r.read_i64()?;
    // Upstream wraps these two reads in try/catch and keeps them null on
    // failure; replicate as best-effort reads.
    let (server_ip, server_port) = read_optional_endpoint(r);
    Ok(PingPacket {
        time,
        server_ip,
        server_port,
    })
}

fn read_optional_endpoint(r: &mut WireReader) -> (Option<String>, Option<u16>) {
    let start = r.pos();
    match (r.read_safe_utf(255), r.read_u16()) {
        (Ok(ip), Ok(port)) => (Some(ip), Some(port)),
        _ => {
            // restore position like a transaction so downstream readers (if
            // any) behave as if the optional tail was never read
            let _ = r.set_pos(start);
            (None, None)
        }
    }
}

fn read_base_audio(r: &mut WireReader) -> Result<(i64, Vec<u8>)> {
    let sequence_number = r.read_i64()?;
    let n = r.read_safe_int(1, 2048)? as usize;
    let data = r.read_bytes(n)?;
    Ok((sequence_number, data))
}

fn read_player_audio(r: &mut WireReader) -> Result<PlayerAudioPacket> {
    let (sequence_number, data) = read_base_audio(r)?;
    Ok(PlayerAudioPacket {
        sequence_number,
        data,
        activation_id: r.read_uuid()?,
        distance: r.read_i16()?,
        stereo: r.read_bool()?,
    })
}

fn read_source_audio(r: &mut WireReader) -> Result<SourceAudioPacket> {
    let (sequence_number, data) = read_base_audio(r)?;
    Ok(SourceAudioPacket {
        sequence_number,
        data,
        source_id: r.read_uuid()?,
        source_state: r.read_i8()?,
        distance: r.read_i16()?,
    })
}

fn read_self_audio_info(r: &mut WireReader) -> Result<SelfAudioInfoPacket> {
    Ok(SelfAudioInfoPacket {
        source_id: r.read_uuid()?,
        sequence_number: r.read_i64()?,
        data: if r.read_bool()? {
            let n = r.read_safe_int(1, 2048)? as usize;
            Some(r.read_bytes(n)?)
        } else {
            None
        },
        distance: r.read_i16()?,
    })
}

fn read_custom(r: &mut WireReader) -> Result<CustomPacket> {
    // Upstream `read` only consumes the addon id.
    Ok(CustomPacket {
        addon_id: r.read_utf()?,
        payload: None,
    })
}

/// Decoded UDP frame header; the body is kept raw so it can be parsed lazily,
/// exactly like `PacketUdp` defers `packet.read(input)` to `getPacket()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpEnvelope<'a> {
    /// Read as `readByte()`; `Custom`'s `0x100` arrives as `0x00`.
    pub id: u8,
    pub secret: Uuid,
    pub timestamp: i64,
    body: &'a [u8],
}

impl<'a> UdpEnvelope<'a> {
    /// Parses the 25-byte header. `None` when the magic number is wrong
    /// (upstream returns `Optional.empty` for non-PV packets).
    pub fn decode(data: &'a [u8]) -> Result<Option<Self>> {
        let mut r = WireReader::new(data);
        let magic = r.read_u32()?;
        if magic != UDP_MAGIC {
            return Ok(None);
        }
        let id = r.read_i8()? as u8;
        let secret = r.read_uuid()?;
        let timestamp = r.read_i64()?;
        Ok(Some(Self {
            id,
            secret,
            timestamp,
            body: &data[r.pos()..],
        }))
    }

    /// Parses the body (like `PacketUdp.getPacket()`). `None` when the id is
    /// unknown or registered for a different direction.
    pub fn decode_packet(&self, direction: PacketDirection) -> Result<Option<UdpPacket>> {
        let factory = match udp_entries()
            .into_iter()
            .find(|(i, dir, _)| *i == self.id as u32 && dir.accepts(direction))
        {
            Some((_, _, f)) => f,
            None => return Ok(None),
        };
        let mut r = WireReader::new(self.body);
        factory(&mut r).map(Some)
    }

    /// Raw body slice after the header.
    pub fn body(&self) -> &'a [u8] {
        self.body
    }
}

/// `PacketUdpCodec`: frames UDP packets as
/// `magic + type byte + secret UUID + timestamp + body`.
#[derive(Debug, Clone, Default)]
pub struct UdpCodec {
    registry: PacketRegistry,
}

impl UdpCodec {
    pub fn new() -> Self {
        Self {
            registry: PacketRegistry::new(),
        }
    }

    pub fn registry(&self) -> &PacketRegistry {
        &self.registry
    }

    /// `PacketUdpCodec.encode`: body of `writeInt(MAGIC) + writeByte(type) +
    /// writeUUID(secret) + writeLong(timestamp) + body`.
    pub fn encode(&self, packet: &UdpPacket, secret: Uuid, timestamp: i64) -> Result<Vec<u8>> {
        let mut out = WireWriter::new();
        out.write_u32(UDP_MAGIC);
        out.write_i8(packet.id() as i8);
        out.write_uuid(secret);
        out.write_i64(timestamp);
        packet.write_body(&mut out)?;
        Ok(out.into_inner())
    }

    /// Convenience: decode the header and parse the body in one step.
    ///
    /// `Ok(None)` when the magic is wrong or the id is unknown / wrong
    /// direction; `Err` when the body itself is malformed.
    pub fn decode(&self, data: &[u8], direction: PacketDirection) -> Result<Option<UdpPacket>> {
        match UdpEnvelope::decode(data)? {
            Some(envelope) => envelope.decode_packet(direction),
            None => Ok(None),
        }
    }

    /// Header-only decode (lazy body), mirroring the upstream
    /// `decode(ByteArrayDataInput, PacketDirection)` which never touches the
    /// packet body.
    pub fn decode_header<'a>(&self, data: &'a [u8]) -> Result<Option<UdpEnvelope<'a>>> {
        UdpEnvelope::decode(data)
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{CodecInfo, VoiceActivation, VoiceSourceLine};

    fn codec() -> TcpCodec {
        TcpCodec::new()
    }

    fn sample_source_info() -> SourceInfo {
        let base = crate::data::SourceInfoBase {
            addon_id: "pv".to_string(),
            id: Uuid::from_u128(0xABCD),
            name: Some("added".to_string()),
            state: 1,
            decoder_info: Some(CodecInfo {
                name: "opus".to_string(),
                params: vec![("b".to_string(), "1".to_string())],
            }),
            stereo: false,
            line_id: Uuid::from_u128(0x1234),
            icon_visible: true,
            angle: 45,
        };
        SourceInfo::Player(crate::data::PlayerSourceInfo {
            player_info: VoicePlayerInfo {
                player_id: Uuid::from_u128(0x9999),
                player_nick: "nick".to_string(),
                muted: false,
                voice_disabled: false,
                microphone_muted: true,
            },
            base,
        })
    }

    fn sample_config() -> ConfigPacket {
        ConfigPacket {
            server_id: Uuid::from_u128(0xCAFE),
            capture_info: CaptureInfo {
                sample_rate: 48000,
                mtu_size: 1024,
                encoder_info: Some(CodecInfo {
                    name: "opus".to_string(),
                    params: vec![("bitrate".to_string(), "32000".to_string())],
                }),
            },
            encryption: Some(EncryptionInfo {
                algorithm: "AES".to_string(),
                data: vec![1, 2, 3, 4],
            }),
            source_lines: vec![VoiceSourceLine::new(
                "proximity".to_string(),
                "t".to_string(),
                "i".to_string(),
                0.7,
                5,
                None,
            )],
            activations: vec![VoiceActivation::new(
                "proximity".to_string(),
                "t".to_string(),
                "i".to_string(),
                vec![-1, 16, 32],
                16,
                true,
                true,
                false,
                None,
                10,
            )],
            permissions: vec![("pv.speak".to_string(), true)],
            player_icon_config: Some(PlayerIconConfig::new(
                vec![crate::data::PlayerIconVisibility::HideClientMuted],
                Pos3d::new(0.0, 0.5, 0.0),
            )),
        }
    }

    #[test]
    fn tcp_roundtrip_all_packets() {
        let packets = vec![
            TcpPacket::Connection(ConnectionPacket {
                secret: Uuid::from_u128(1),
                ip: "127.0.0.1".to_string(),
                port: 25565,
            }),
            TcpPacket::PlayerInfoRequest(PlayerInfoRequestPacket),
            TcpPacket::Config(sample_config()),
            TcpPacket::ConfigPlayerInfo(ConfigPlayerInfoPacket {
                permissions: vec![("a".to_string(), true), ("b".to_string(), false)],
            }),
            TcpPacket::LanguageRequest(LanguageRequestPacket {
                language: "en_us".to_string(),
            }),
            TcpPacket::Language(LanguagePacket {
                language_name: "English".to_string(),
                language: vec![("key".to_string(), "value".to_string())],
            }),
            TcpPacket::PlayerList(PlayerListPacket {
                players: vec![VoicePlayerInfo {
                    player_id: Uuid::from_u128(2),
                    player_nick: "p".to_string(),
                    muted: true,
                    voice_disabled: false,
                    microphone_muted: false,
                }],
            }),
            TcpPacket::PlayerInfoUpdate(PlayerInfoUpdatePacket {
                player_info: VoicePlayerInfo {
                    player_id: Uuid::from_u128(3),
                    player_nick: "q".to_string(),
                    muted: false,
                    voice_disabled: true,
                    microphone_muted: true,
                },
            }),
            TcpPacket::PlayerDisconnect(PlayerDisconnectPacket {
                player_id: Uuid::from_u128(4),
            }),
            TcpPacket::PlayerInfo(PlayerInfoPacket {
                voice_disabled: false,
                microphone_muted: false,
                minecraft_version: "1.21".to_string(),
                version: "2.1.7".to_string(),
                public_key: vec![9, 8, 7, 6, 5],
            }),
            TcpPacket::PlayerState(PlayerStatePacket {
                voice_disabled: true,
                microphone_muted: false,
            }),
            TcpPacket::PlayerAudioEnd(PlayerAudioEndPacket {
                sequence_number: 777,
                activation_id: Uuid::from_u128(5),
                distance: 42,
            }),
            TcpPacket::PlayerActivationDistances(PlayerActivationDistancesPacket {
                distance_by_activation_id: vec![(Uuid::from_u128(6), 24)],
            }),
            TcpPacket::DistanceVisualize(DistanceVisualizePacket {
                radius: 16,
                hex_color: 0xFF0000,
                position: Some(Pos3d::new(1.0, 2.0, 3.0)),
            }),
            TcpPacket::SourceInfoRequest(SourceInfoRequestPacket {
                source_id: Uuid::from_u128(7),
            }),
            TcpPacket::SourceInfo(SourceInfoPacket {
                source_info: sample_source_info(),
            }),
            TcpPacket::SelfSourceInfo(SelfSourceInfoPacket {
                source_info: SelfSourceInfo {
                    source_info: sample_source_info(),
                    player_id: Uuid::from_u128(8),
                    activation_id: Uuid::from_u128(9),
                    sequence_number: 10,
                },
            }),
            TcpPacket::SourceAudioEnd(SourceAudioEndPacket {
                source_id: Uuid::from_u128(10),
                sequence_number: 11,
            }),
            TcpPacket::ActivationRegister(ActivationRegisterPacket {
                activation: VoiceActivation::new(
                    "proximity".to_string(),
                    "t".to_string(),
                    "i".to_string(),
                    vec![8, 16, 32],
                    16,
                    true,
                    false,
                    false,
                    None,
                    3,
                ),
            }),
            TcpPacket::ActivationUnregister(ActivationUnregisterPacket {
                activation_id: Uuid::from_u128(11),
            }),
            TcpPacket::SourceLineRegister(SourceLineRegisterPacket {
                source_line: VoiceSourceLine::new(
                    "proximity".to_string(),
                    "t".to_string(),
                    "i".to_string(),
                    0.9,
                    4,
                    None,
                ),
            }),
            TcpPacket::SourceLineUnregister(SourceLineUnregisterPacket {
                line_id: Uuid::from_u128(12),
            }),
            TcpPacket::SourceLinePlayerAdd(SourceLinePlayerAddPacket {
                line_id: Uuid::from_u128(13),
                player: McGameProfile {
                    id: Uuid::from_u128(14),
                    name: "n".to_string(),
                    properties: vec![],
                },
            }),
            TcpPacket::SourceLinePlayerRemove(SourceLinePlayerRemovePacket {
                line_id: Uuid::from_u128(15),
                player_id: Uuid::from_u128(16),
            }),
            TcpPacket::SourceLinePlayersList(SourceLinePlayersListPacket {
                line_id: Uuid::from_u128(17),
                players: vec![McGameProfile {
                    id: Uuid::from_u128(18),
                    name: "m".to_string(),
                    properties: vec![],
                }],
            }),
            TcpPacket::AnimatedActionBar(AnimatedActionBarPacket {
                json_component: "{\"text\":\"hi\"}".to_string(),
            }),
        ];

        for packet in &packets {
            let encoded = codec().encode(packet).unwrap();
            let decoded = codec().decode(&encoded, packet.direction()).unwrap().unwrap();
            assert_eq!(&decoded, packet, "packet id {:#04x}", packet.id());
        }
    }

    #[test]
    fn tcp_unknown_id_is_none() {
        let data = [0x7F];
        assert!(codec().decode(&data, PacketDirection::Client).unwrap().is_none());
    }

    #[test]
    fn tcp_wrong_direction_is_none() {
        let packet = TcpPacket::Connection(ConnectionPacket {
            secret: Uuid::from_u128(1),
            ip: "x".to_string(),
            port: 1,
        });
        let encoded = codec().encode(&packet).unwrap();
        // ConnectionPacket is CLIENT-bound; decoding from the server side must
        // produce None.
        assert!(codec()
            .decode(&encoded, PacketDirection::Server)
            .unwrap()
            .is_none());
        assert!(codec()
            .decode(&encoded, PacketDirection::Client)
            .unwrap()
            .is_some());
    }

    #[test]
    fn config_without_player_icon_config_still_decodes() {
        let packet = TcpPacket::Config(ConfigPacket {
            player_icon_config: None,
            ..sample_config()
        });
        let encoded = codec().encode(&packet).unwrap();
        let decoded = codec()
            .decode(&encoded, PacketDirection::Client)
            .unwrap()
            .unwrap();
        match decoded {
            TcpPacket::Config(c) => {
                assert!(c.player_icon_config.is_none());
                assert_eq!(c.server_id, sample_config().server_id);
            }
            other => panic!("expected config, got {other:?}"),
        }
    }

    #[test]
    fn udp_header_format() {
        let packet = UdpPacket::Ping(PingPacket {
            time: 1234,
            server_ip: None,
            server_port: None,
        });
        let secret = Uuid::from_u128(0xAABB);
        let codec = UdpCodec::new();
        let encoded = codec.encode(&packet, secret, 999).unwrap();
        assert_eq!(&encoded[0..4], &0x4e90_04e9u32.to_be_bytes());
        assert_eq!(encoded[4], 0x01);
        assert_eq!(&encoded[5..21], secret.as_bytes());
        assert_eq!(&encoded[21..29], &999i64.to_be_bytes());
        assert_eq!(&encoded[29..37], &1234i64.to_be_bytes());

        let decoded = codec.decode(&encoded, PacketDirection::Any).unwrap().unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn udp_bad_magic_is_none() {
        let codec = UdpCodec::new();
        let mut junk = vec![0u8; 29];
        junk[0] = 0x01;
        assert!(codec.decode(&junk, PacketDirection::Any).unwrap().is_none());
    }

    #[test]
    fn udp_player_audio_roundtrip() {
        let packet = UdpPacket::PlayerAudio(PlayerAudioPacket {
            sequence_number: 5,
            data: vec![1, 2, 3, 4],
            activation_id: Uuid::from_u128(0x11),
            distance: 30,
            stereo: false,
        });
        let codec = UdpCodec::new();
        let encoded = codec.encode(&packet, Uuid::from_u128(1), 2).unwrap();
        let decoded = codec
            .decode(&encoded, PacketDirection::Server)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, packet);
        // wrong direction -> None
        assert!(codec
            .decode(&encoded, PacketDirection::Client)
            .unwrap()
            .is_none());
    }

    #[test]
    fn udp_custom_id_truncates() {
        let packet = UdpPacket::Custom(CustomPacket {
            addon_id: "plugin".to_string(),
            payload: Some(vec![9, 9]),
        });
        let codec = UdpCodec::new();
        let encoded = codec.encode(&packet, Uuid::from_u128(1), 2).unwrap();
        // 0x100 as writeByte -> 0x00 on the wire
        assert_eq!(encoded[4], 0x00);
        // decoding finds no packet for id 0 (like upstream), so None
        assert!(codec
            .decode(&encoded, PacketDirection::Any)
            .unwrap()
            .is_none());
    }

    #[test]
    fn udp_envelope_lazy_body() {
        let packet = UdpPacket::SourceAudio(SourceAudioPacket {
            sequence_number: 7,
            data: vec![5, 5, 5],
            source_id: Uuid::from_u128(0x22),
            source_state: 1,
            distance: 12,
        });
        let codec = UdpCodec::new();
        let encoded = codec.encode(&packet, Uuid::from_u128(3), 4).unwrap();
        let envelope = codec.decode_header(&encoded).unwrap().unwrap();
        assert_eq!(envelope.id, 0x03);
        assert_eq!(envelope.secret, Uuid::from_u128(3));
        assert_eq!(envelope.timestamp, 4);
        let decoded = envelope.decode_packet(PacketDirection::Client).unwrap().unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn ping_optional_tail_roundtrip() {
        let packet = UdpPacket::Ping(PingPacket {
            time: 42,
            server_ip: Some("example.com".to_string()),
            server_port: Some(25565),
        });
        let codec = UdpCodec::new();
        let encoded = codec.encode(&packet, Uuid::from_u128(1), 0).unwrap();
        let decoded = codec.decode(&encoded, PacketDirection::Any).unwrap().unwrap();
        assert_eq!(decoded, packet);

        // short body without the tail still decodes
        let bare = codec
            .encode(
                &UdpPacket::Ping(PingPacket {
                    time: 42,
                    server_ip: None,
                    server_port: None,
                }),
                Uuid::from_u128(1),
                0,
            )
            .unwrap();
        let decoded = codec.decode(&bare, PacketDirection::Any).unwrap().unwrap();
        match decoded {
            UdpPacket::Ping(p) => {
                assert_eq!(p.time, 42);
                assert!(p.server_ip.is_none());
            }
            other => panic!("expected ping, got {other:?}"),
        }
    }

    #[test]
    fn registry_ids() {
        let registry = PacketRegistry::new();
        assert_eq!(registry.tcp_direction(0x01), Some(PacketDirection::Client));
        assert_eq!(registry.tcp_direction(0x0A), Some(PacketDirection::Server));
        assert_eq!(registry.tcp_direction(0x30), None);
        assert_eq!(registry.udp_direction(0x01), Some(PacketDirection::Any));
        assert_eq!(registry.udp_direction(0x100), Some(PacketDirection::Any));
        assert_eq!(
            registry.get_udp_id(&UdpPacket::Custom(CustomPacket {
                addon_id: "x".to_string(),
                payload: None,
            })),
            0x100
        );
        let p = TcpPacket::AnimatedActionBar(AnimatedActionBarPacket {
            json_component: String::new(),
        });
        assert_eq!(registry.get_tcp_id(&p), 0x1A);
    }
}