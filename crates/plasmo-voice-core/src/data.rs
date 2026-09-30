//! Serializable data models of the Plasmo Voice protocol.
//!
//! Every type here mirrors a `PacketSerializable` (or serializer) class from
//! `su.plo.voice.proto`. Field order and bounds follow the Java/Kotlin source
//! line by line so the wire format matches byte for byte.

use uuid::Uuid;

use crate::error::{Result, VoiceError};
use crate::md5::name_uuid_from_bytes;
use crate::util::{WireReader, WireWriter};

// ---------------------------------------------------------------------------
// Pos3d
// ---------------------------------------------------------------------------

/// A 3D position; wire format is three big-endian doubles (`Pos3dSerializer`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pos3d {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Pos3d {
    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    pub const fn zero() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        }
    }

    pub fn serialize(&self, out: &mut WireWriter) {
        out.write_f64(self.x);
        out.write_f64(self.y);
        out.write_f64(self.z);
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        Ok(Self {
            x: input.read_f64()?,
            y: input.read_f64()?,
            z: input.read_f64()?,
        })
    }
}

// ---------------------------------------------------------------------------
// McGameProfile
// ---------------------------------------------------------------------------

/// Property of a [`McGameProfile`]; on the wire the signature is always a
/// string (`null` is written as the empty string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McGameProfileProperty {
    pub name: String,
    pub value: String,
    pub signature: Option<String>,
}

/// A Minecraft game profile (`McGameProfileSerializer`).
///
/// Wire format: UUID id, modified-UTF-8 name, int count `[0, 100]`, then per
/// property: name, value, signature (all modified-UTF-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McGameProfile {
    pub id: Uuid,
    pub name: String,
    pub properties: Vec<McGameProfileProperty>,
}

impl McGameProfile {
    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_uuid(self.id);
        out.write_utf(&self.name)?;
        out.write_i32(self.properties.len() as i32);
        for p in &self.properties {
            out.write_utf(&p.name)?;
            out.write_utf(&p.value)?;
            out.write_utf(p.signature.as_deref().unwrap_or(""))?;
        }
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        let id = input.read_uuid()?;
        let name = input.read_utf()?;
        let n = input.read_safe_int(0, 100)? as usize;
        let mut properties = Vec::with_capacity(n);
        for _ in 0..n {
            properties.push(McGameProfileProperty {
                name: input.read_utf()?,
                value: input.read_utf()?,
                signature: Some(input.read_utf()?),
            });
        }
        Ok(Self {
            id,
            name,
            properties,
        })
    }
}

// ---------------------------------------------------------------------------
// PlayerIconVisibility / PlayerIconConfig
// ---------------------------------------------------------------------------

/// Flags controlling icon visibility above players (2.1.7+).
///
/// Serialized by the enum constant name via modified-UTF-8 (`valueOf` on read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerIconVisibility {
    /// Hides the "No Mod Installed" icon.
    HideNotInstalled,
    /// Hides the "Player Muted Audio" icon (client disabled voice chat).
    HideVoiceChatDisabled,
    /// Hides the "Server Muted" icon.
    HideServerMuted,
    /// Hides the "Client Muted" icon (player muted in the volume tab).
    HideClientMuted,
    /// Hides the "Player Client Audio" icon (activated player source).
    HideSourceIcon,
}

impl PlayerIconVisibility {
    pub fn name(self) -> &'static str {
        match self {
            PlayerIconVisibility::HideNotInstalled => "HIDE_NOT_INSTALLED",
            PlayerIconVisibility::HideVoiceChatDisabled => "HIDE_VOICE_CHAT_DISABLED",
            PlayerIconVisibility::HideServerMuted => "HIDE_SERVER_MUTED",
            PlayerIconVisibility::HideClientMuted => "HIDE_CLIENT_MUTED",
            PlayerIconVisibility::HideSourceIcon => "HIDE_SOURCE_ICON",
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "HIDE_NOT_INSTALLED" => Ok(PlayerIconVisibility::HideNotInstalled),
            "HIDE_VOICE_CHAT_DISABLED" => Ok(PlayerIconVisibility::HideVoiceChatDisabled),
            "HIDE_SERVER_MUTED" => Ok(PlayerIconVisibility::HideServerMuted),
            "HIDE_CLIENT_MUTED" => Ok(PlayerIconVisibility::HideClientMuted),
            "HIDE_SOURCE_ICON" => Ok(PlayerIconVisibility::HideSourceIcon),
            other => Err(VoiceError::UnknownEnumName(
                "PlayerIconVisibility",
                other.to_string(),
            )),
        }
    }
}

/// Icon visibility configuration sent in `ConfigPacket` (2.1.7+).
///
/// Wire format: int count `[0, 5]`, then count enum names, then a [`Pos3d`]
/// icon offset.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerIconConfig {
    pub icon_visibility: Vec<PlayerIconVisibility>,
    pub icon_offset: Pos3d,
}

impl PlayerIconConfig {
    pub fn new(icon_visibility: Vec<PlayerIconVisibility>, icon_offset: Pos3d) -> Self {
        Self {
            icon_visibility,
            icon_offset,
        }
    }

    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_i32(self.icon_visibility.len() as i32);
        for v in &self.icon_visibility {
            out.write_utf(v.name())?;
        }
        self.icon_offset.serialize(out);
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        let n = input.read_safe_int(0, 5)? as usize;
        let mut icon_visibility = Vec::with_capacity(n);
        for _ in 0..n {
            icon_visibility.push(PlayerIconVisibility::from_name(&input.read_utf()?)?);
        }
        let icon_offset = Pos3d::deserialize(input)?;
        Ok(Self {
            icon_visibility,
            icon_offset,
        })
    }
}

// ---------------------------------------------------------------------------
// CodecInfo
// ---------------------------------------------------------------------------

/// Codec metadata (name + parameters); parameters are encoded as a count
/// bounded by `[0, 128]` followed by key/value string pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecInfo {
    pub name: String,
    pub params: Vec<(String, String)>,
}

impl CodecInfo {
    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_utf(&self.name)?;
        out.write_i32(self.params.len() as i32);
        for (k, v) in &self.params {
            out.write_utf(k)?;
            out.write_utf(v)?;
        }
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        let name = input.read_utf()?;
        let n = input.read_safe_int(0, 128)? as usize;
        let mut params = Vec::with_capacity(n);
        for _ in 0..n {
            params.push((input.read_utf()?, input.read_utf()?));
        }
        Ok(Self { name, params })
    }
}

// ---------------------------------------------------------------------------
// CaptureInfo / EncryptionInfo
// ---------------------------------------------------------------------------

/// Client capture configuration: fixed ints plus an optional encoder codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureInfo {
    pub sample_rate: i32,
    pub mtu_size: i32,
    pub encoder_info: Option<CodecInfo>,
}

impl CaptureInfo {
    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_i32(self.sample_rate);
        out.write_i32(self.mtu_size);
        out.write_bool(self.encoder_info.is_some());
        if let Some(encoder) = &self.encoder_info {
            encoder.serialize(out)?;
        }
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        let sample_rate = input.read_i32()?;
        let mtu_size = input.read_i32()?;
        let encoder_info = if input.read_bool()? {
            Some(CodecInfo::deserialize(input)?)
        } else {
            None
        };
        Ok(Self {
            sample_rate,
            mtu_size,
            encoder_info,
        })
    }
}

/// Encryption metadata: algorithm name plus key data, byte length `[1, 2048]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionInfo {
    pub algorithm: String,
    pub data: Vec<u8>,
}

impl EncryptionInfo {
    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_utf(&self.algorithm)?;
        out.write_i32(self.data.len() as i32);
        out.write_bytes(&self.data);
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        let algorithm = input.read_utf()?;
        let n = input.read_safe_int(1, 2048)? as usize;
        let data = input.read_bytes(n)?;
        Ok(Self { algorithm, data })
    }
}

// ---------------------------------------------------------------------------
// VoicePlayerInfo
// ---------------------------------------------------------------------------

/// Player information sent in the player list / updates and config.
///
/// Wire format: UUID playerId, modified-UTF-8 playerNick, then `muted`,
/// `voiceDisabled`, `microphoneMuted` booleans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicePlayerInfo {
    pub player_id: Uuid,
    pub player_nick: String,
    pub muted: bool,
    pub voice_disabled: bool,
    pub microphone_muted: bool,
}

impl VoicePlayerInfo {
    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_uuid(self.player_id);
        out.write_utf(&self.player_nick)?;
        out.write_bool(self.muted);
        out.write_bool(self.voice_disabled);
        out.write_bool(self.microphone_muted);
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        Ok(Self {
            player_id: input.read_uuid()?,
            player_nick: input.read_utf()?,
            muted: input.read_bool()?,
            voice_disabled: input.read_bool()?,
            microphone_muted: input.read_bool()?,
        })
    }
}

// ---------------------------------------------------------------------------
// VoiceActivation
// ---------------------------------------------------------------------------

/// A voice activation (e.g. proximity or a push-to-talk key).
///
/// The activation id is **not** on the wire; it is derived from the name via
/// `UUID.nameUUIDFromBytes(name + "_activation")` (see [`Self::generate_id`]).
///
/// Field order: name, translation, icon, distances (int list, max 64
/// elements), defaultDistance, proximity, transitive, stereoSupported,
/// optional encoder CodecInfo, weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceActivation {
    /// Derived from `name`; never serialized.
    pub id: Uuid,
    pub name: String,
    pub translation: String,
    pub icon: String,
    pub distances: Vec<i32>,
    pub default_distance: i32,
    pub proximity: bool,
    pub transitive: bool,
    pub stereo_supported: bool,
    pub encoder_info: Option<CodecInfo>,
    pub weight: i32,
}

impl VoiceActivation {
    pub const PROXIMITY_NAME: &'static str = "proximity";

    /// `VoiceActivation.generateId`: MD5 of `name + "_activation"` with the
    /// RFC 4122 version/variant bits set, **without** a UUID namespace.
    pub fn generate_id(name: &str) -> Uuid {
        let mut buf = String::with_capacity(name.len() + 11);
        buf.push_str(name);
        buf.push_str("_activation");
        name_uuid_from_bytes(buf.as_bytes())
    }

    /// Replicates `validateDefaultDistance`:
    /// empty -> 0; `[-1, max]` -> clamp into `[1, max]` else `max/2`;
    /// member -> itself; otherwise the middle element of `distances`.
    pub fn validate_default_distance(distances: &[i32], default_distance: i32) -> i32 {
        if distances.is_empty() {
            return 0;
        }
        if distances.len() == 2 && distances[0] == -1 {
            if (1..=distances[1]).contains(&default_distance) {
                return default_distance;
            } else {
                return distances[1] / 2;
            }
        }
        if distances.contains(&default_distance) {
            default_distance
        } else {
            distances[distances.len() / 2]
        }
    }

    pub fn new(
        name: String,
        translation: String,
        icon: String,
        distances: Vec<i32>,
        default_distance: i32,
        proximity: bool,
        stereo_supported: bool,
        transitive: bool,
        encoder_info: Option<CodecInfo>,
        weight: i32,
    ) -> Self {
        let id = Self::generate_id(&name);
        let default_distance = Self::validate_default_distance(&distances, default_distance);
        Self {
            id,
            name,
            translation,
            icon,
            distances,
            default_distance,
            proximity,
            stereo_supported,
            transitive,
            encoder_info,
            weight,
        }
    }

    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_utf(&self.name)?;
        out.write_utf(&self.translation)?;
        out.write_utf(&self.icon)?;
        out.write_i32(self.distances.len() as i32);
        for d in &self.distances {
            out.write_i32(*d);
        }
        out.write_i32(self.default_distance);
        out.write_bool(self.proximity);
        out.write_bool(self.transitive);
        out.write_bool(self.stereo_supported);
        out.write_bool(self.encoder_info.is_some());
        if let Some(encoder) = &self.encoder_info {
            encoder.serialize(out)?;
        }
        out.write_i32(self.weight);
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        let name = input.read_utf()?;
        let translation = input.read_utf()?;
        let icon = input.read_utf()?;
        let id = Self::generate_id(&name);
        let distances = {
            let n = input.read_safe_int(0, 64)? as usize;
            let mut d = Vec::with_capacity(n);
            for _ in 0..n {
                d.push(input.read_i32()?);
            }
            d
        };
        let default_distance = Self::validate_default_distance(&distances, input.read_i32()?);
        let proximity = input.read_bool()?;
        let transitive = input.read_bool()?;
        let stereo_supported = input.read_bool()?;
        let encoder_info = if input.read_bool()? {
            Some(CodecInfo::deserialize(input)?)
        } else {
            None
        };
        let weight = input.read_i32()?;
        Ok(Self {
            id,
            name,
            translation,
            icon,
            distances,
            default_distance,
            proximity,
            transitive,
            stereo_supported,
            encoder_info,
            weight,
        })
    }
}

// ---------------------------------------------------------------------------
// VoiceSourceLine
// ---------------------------------------------------------------------------

/// A communicable source line (e.g. proximity, global or a custom line).
///
/// Like activations, the line id is derived from the name via
/// `UUID.nameUUIDFromBytes(name + "_line")` and is never serialized.
///
/// Wire format: name, translation, icon, defaultVolume (f64), weight (i32),
/// `hasPlayers` bool, and when present an int count `[0, Short.MAX]` followed
/// by that many [`McGameProfile`]s.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceSourceLine {
    /// Derived from `name`; never serialized.
    pub id: Uuid,
    pub name: String,
    pub translation: String,
    pub icon: String,
    pub default_volume: f64,
    pub weight: i32,
    pub players: Option<Vec<McGameProfile>>,
}

impl VoiceSourceLine {
    pub const PROXIMITY_NAME: &'static str = "proximity";

    /// `VoiceSourceLine.generateId`: MD5 of `name + "_line"` with the RFC 4122
    /// version/variant bits set, **without** a UUID namespace.
    pub fn generate_id(name: &str) -> Uuid {
        let mut buf = String::with_capacity(name.len() + 5);
        buf.push_str(name);
        buf.push_str("_line");
        name_uuid_from_bytes(buf.as_bytes())
    }

    pub fn new(
        name: String,
        translation: String,
        icon: String,
        default_volume: f64,
        weight: i32,
        players: Option<Vec<McGameProfile>>,
    ) -> Self {
        let id = Self::generate_id(&name);
        let default_volume = default_volume.clamp(0.0, 1.0);
        Self {
            id,
            name,
            translation,
            icon,
            default_volume,
            weight,
            players,
        }
    }

    pub fn has_players(&self) -> bool {
        self.players.is_some()
    }

    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_utf(&self.name)?;
        out.write_utf(&self.translation)?;
        out.write_utf(&self.icon)?;
        out.write_f64(self.default_volume);
        out.write_i32(self.weight);
        out.write_bool(self.has_players());
        if let Some(players) = &self.players {
            out.write_i32(players.len() as i32);
            for p in players {
                p.serialize(out)?;
            }
        }
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        let name = input.read_utf()?;
        let id = Self::generate_id(&name);
        let translation = input.read_utf()?;
        let icon = input.read_utf()?;
        let default_volume = input.read_f64()?;
        let weight = input.read_i32()?;
        let players = if input.read_bool()? {
            let n = input.read_safe_int(0, i16::MAX as i32)? as usize;
            let mut list = Vec::with_capacity(n);
            for _ in 0..n {
                list.push(McGameProfile::deserialize(input)?);
            }
            Some(list)
        } else {
            None
        };
        Ok(Self {
            id,
            name,
            translation,
            icon,
            default_volume,
            weight,
            players,
        })
    }
}

// ---------------------------------------------------------------------------
// SourceInfo family
// ---------------------------------------------------------------------------

/// Common base payload of every [`SourceInfo`] variant.
///
/// Wire format (after the type-name tag): addonId (utf), id (UUID),
/// nullable name, state (i8), optional decoder CodecInfo, stereo (bool),
/// lineId (UUID), iconVisible (bool), angle (i32).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfoBase {
    pub addon_id: String,
    pub id: Uuid,
    pub name: Option<String>,
    pub state: i8,
    pub decoder_info: Option<CodecInfo>,
    pub stereo: bool,
    pub line_id: Uuid,
    pub icon_visible: bool,
    pub angle: i32,
}

impl SourceInfoBase {
    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_utf(&self.addon_id)?;
        out.write_uuid(self.id);
        out.write_optional_string(self.name.as_deref())?;
        out.write_i8(self.state);
        out.write_bool(self.decoder_info.is_some());
        if let Some(decoder) = &self.decoder_info {
            decoder.serialize(out)?;
        }
        out.write_bool(self.stereo);
        out.write_uuid(self.line_id);
        out.write_bool(self.icon_visible);
        out.write_i32(self.angle);
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        Ok(Self {
            addon_id: input.read_utf()?,
            id: input.read_uuid()?,
            name: input.read_optional_string()?,
            state: input.read_i8()?,
            decoder_info: if input.read_bool()? {
                Some(CodecInfo::deserialize(input)?)
            } else {
                None
            },
            stereo: input.read_bool()?,
            line_id: input.read_uuid()?,
            icon_visible: input.read_bool()?,
            angle: input.read_i32()?,
        })
    }
}

/// Player source: base + a full [`VoicePlayerInfo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerSourceInfo {
    pub base: SourceInfoBase,
    pub player_info: VoicePlayerInfo,
}

/// Entity source: base + an int entity id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitySourceInfo {
    pub base: SourceInfoBase,
    pub entity_id: i32,
}

/// Static source: base + position and look angle.
#[derive(Debug, Clone, PartialEq)]
pub struct StaticSourceInfo {
    pub base: SourceInfoBase,
    pub position: Pos3d,
    pub look_angle: Pos3d,
}

/// Direct source: base + optional sender profile and relative position.
///
/// Note: the upstream `DirectSourceInfo` also carries a `lookAngle` field that
/// is intentionally never serialized — it is not part of the wire format.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectSourceInfo {
    pub base: SourceInfoBase,
    pub sender: Option<McGameProfile>,
    pub relative_position: Option<Pos3d>,
    pub camera_relative: bool,
    /// Rust-side extra matching the upstream field; **not** on the wire.
    pub look_angle: Pos3d,
}

/// Any source info; the wire format starts with the type name
/// (`PLAYER`/`ENTITY`/`STATIC`/`DIRECT`) which drives the dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceInfo {
    Player(PlayerSourceInfo),
    Entity(EntitySourceInfo),
    Static(StaticSourceInfo),
    Direct(DirectSourceInfo),
}

impl SourceInfo {
    pub fn type_name(&self) -> &'static str {
        match self {
            SourceInfo::Player(_) => "PLAYER",
            SourceInfo::Entity(_) => "ENTITY",
            SourceInfo::Static(_) => "STATIC",
            SourceInfo::Direct(_) => "DIRECT",
        }
    }

    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        out.write_utf(self.type_name())?;
        match self {
            SourceInfo::Player(info) => {
                info.base.serialize(out)?;
                info.player_info.serialize(out)?;
            }
            SourceInfo::Entity(info) => {
                info.base.serialize(out)?;
                out.write_i32(info.entity_id);
            }
            SourceInfo::Static(info) => {
                info.base.serialize(out)?;
                info.position.serialize(out);
                info.look_angle.serialize(out);
            }
            SourceInfo::Direct(info) => {
                info.base.serialize(out)?;
                out.write_bool(info.sender.is_some());
                if let Some(sender) = &info.sender {
                    sender.serialize(out)?;
                }
                out.write_bool(info.relative_position.is_some());
                if let Some(relative) = &info.relative_position {
                    relative.serialize(out);
                }
                out.write_bool(info.camera_relative);
            }
        }
        Ok(())
    }

    /// `SourceInfo.of(in)`: reads the type name first, then the payload.
    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        let type_name = input.read_utf()?;
        match type_name.as_str() {
            "PLAYER" => Ok(SourceInfo::Player(PlayerSourceInfo {
                base: SourceInfoBase::deserialize(input)?,
                player_info: VoicePlayerInfo::deserialize(input)?,
            })),
            "ENTITY" => Ok(SourceInfo::Entity(EntitySourceInfo {
                base: SourceInfoBase::deserialize(input)?,
                entity_id: input.read_i32()?,
            })),
            "STATIC" => Ok(SourceInfo::Static(StaticSourceInfo {
                base: SourceInfoBase::deserialize(input)?,
                position: Pos3d::deserialize(input)?,
                look_angle: Pos3d::deserialize(input)?,
            })),
            "DIRECT" => {
                let base = SourceInfoBase::deserialize(input)?;
                let sender = if input.read_bool()? {
                    Some(McGameProfile::deserialize(input)?)
                } else {
                    None
                };
                let relative_position = if input.read_bool()? {
                    Some(Pos3d::deserialize(input)?)
                } else {
                    None
                };
                let camera_relative = input.read_bool()?;
                Ok(SourceInfo::Direct(DirectSourceInfo {
                    base,
                    sender,
                    relative_position,
                    camera_relative,
                    look_angle: Pos3d::zero(),
                }))
            }
            other => Err(VoiceError::UnknownEnumName(
                "SourceInfo.Type",
                other.to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// SelfSourceInfo
// ---------------------------------------------------------------------------

/// Source info plus the player/activation identifiers and the current audio
/// sequence number, used by `SelfSourceInfoPacket`.
///
/// Wire format: [`SourceInfo`] (with type-name tag), playerId (UUID),
/// activationId (UUID), sequenceNumber (i64).
#[derive(Debug, Clone, PartialEq)]
pub struct SelfSourceInfo {
    pub source_info: SourceInfo,
    pub player_id: Uuid,
    pub activation_id: Uuid,
    pub sequence_number: i64,
}

impl SelfSourceInfo {
    pub fn serialize(&self, out: &mut WireWriter) -> Result<()> {
        self.source_info.serialize(out)?;
        out.write_uuid(self.player_id);
        out.write_uuid(self.activation_id);
        out.write_i64(self.sequence_number);
        Ok(())
    }

    pub fn deserialize(input: &mut WireReader) -> Result<Self> {
        Ok(Self {
            source_info: SourceInfo::deserialize(input)?,
            player_id: input.read_uuid()?,
            activation_id: input.read_uuid()?,
            sequence_number: input.read_i64()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw() -> WireWriter {
        WireWriter::new()
    }

    #[test]
    fn pos3d_roundtrip() {
        let mut w = pw();
        let p = Pos3d::new(1.5, -2.0, 3.25);
        p.serialize(&mut w);
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(Pos3d::deserialize(&mut r).unwrap(), p);
        assert!(r.is_empty());
    }

    #[test]
    fn mc_game_profile_roundtrip() {
        let mut w = pw();
        let profile = McGameProfile {
            id: Uuid::from_u128(0x1234),
            name: "herobrine".to_string(),
            properties: vec![McGameProfileProperty {
                name: "textures".to_string(),
                value: "abc".to_string(),
                signature: Some("sig".to_string()),
            }],
        };
        profile.serialize(&mut w).unwrap();
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(McGameProfile::deserialize(&mut r).unwrap(), profile);
    }

    #[test]
    fn mc_game_profile_null_signature_is_empty() {
        let mut w = pw();
        let profile = McGameProfile {
            id: Uuid::from_u128(0x5678),
            name: "x".to_string(),
            properties: vec![McGameProfileProperty {
                name: "n".to_string(),
                value: "v".to_string(),
                signature: None,
            }],
        };
        profile.serialize(&mut w).unwrap();
        let mut r = WireReader::new(w.as_slice());
        let back = McGameProfile::deserialize(&mut r).unwrap();
        assert_eq!(back.properties[0].signature.as_deref(), Some(""));
    }

    #[test]
    fn player_icon_visibility_names() {
        let names = [
            "HIDE_NOT_INSTALLED",
            "HIDE_VOICE_CHAT_DISABLED",
            "HIDE_SERVER_MUTED",
            "HIDE_CLIENT_MUTED",
            "HIDE_SOURCE_ICON",
        ];
        for name in names {
            let v = PlayerIconVisibility::from_name(name).unwrap();
            assert_eq!(v.name(), name);
        }
        assert!(PlayerIconVisibility::from_name("NOPE").is_err());
    }

    #[test]
    fn player_icon_config_roundtrip() {
        let mut w = pw();
        let cfg = PlayerIconConfig::new(
            vec![
                PlayerIconVisibility::HideClientMuted,
                PlayerIconVisibility::HideSourceIcon,
            ],
            Pos3d::new(0.1, 0.2, 0.3),
        );
        cfg.serialize(&mut w).unwrap();
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(PlayerIconConfig::deserialize(&mut r).unwrap(), cfg);
    }

    #[test]
    fn voice_activation_proximity_id() {
        assert_eq!(
            VoiceActivation::generate_id("proximity").to_string(),
            "4aec07ba-d109-3345-9a0a-92022a116cd0"
        );
    }

    #[test]
    fn voice_activation_roundtrip() {
        let mut w = pw();
        let act = VoiceActivation::new(
            "proximity".to_string(),
            "trans".to_string(),
            "icon".to_string(),
            vec![-1, 16, 32],
            16,
            true,
            false,
            true,
            Some(CodecInfo {
                name: "opus".to_string(),
                params: vec![("bitrate".to_string(), "32000".to_string())],
            }),
            10,
        );
        act.serialize(&mut w).unwrap();
        let mut r = WireReader::new(w.as_slice());
        let back = VoiceActivation::deserialize(&mut r).unwrap();
        assert_eq!(back, act);
        assert_eq!(back.id, VoiceActivation::generate_id("proximity"));
    }

    #[test]
    fn voice_activation_default_distance_validation() {
        assert_eq!(VoiceActivation::validate_default_distance(&[], 10), 0);
        // [-1, max] clamps into [1, max], else max / 2
        assert_eq!(VoiceActivation::validate_default_distance(&[-1, 32], 8), 8);
        assert_eq!(VoiceActivation::validate_default_distance(&[-1, 32], 0), 16);
        assert_eq!(VoiceActivation::validate_default_distance(&[-1, 32], 99), 16);
        // member -> itself
        assert_eq!(VoiceActivation::validate_default_distance(&[8, 16, 32], 16), 16);
        // non-member -> middle element (`distances.get(size / 2)`)
        assert_eq!(VoiceActivation::validate_default_distance(&[8, 16, 32], 24), 16);
        assert_eq!(VoiceActivation::validate_default_distance(&[8, 16, 32, 64], 24), 32);
    }

    #[test]
    fn voice_source_line_roundtrip() {
        let mut w = pw();
        let players = vec![McGameProfile {
            id: Uuid::from_u128(0x99),
            name: "p".to_string(),
            properties: vec![],
        }];
        let line = VoiceSourceLine::new(
            "proximity".to_string(),
            "t".to_string(),
            "i".to_string(),
            0.8,
            5,
            Some(players),
        );
        line.serialize(&mut w).unwrap();
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(VoiceSourceLine::deserialize(&mut r).unwrap(), line);
    }

    #[test]
    fn voice_source_line_volume_clamped() {
        let line = VoiceSourceLine::new(
            "x".to_string(),
            "t".to_string(),
            "i".to_string(),
            1.5,
            1,
            None,
        );
        assert_eq!(line.default_volume, 1.0);
    }

    #[test]
    fn source_info_player_roundtrip() {
        let mut w = pw();
        let base = SourceInfoBase {
            addon_id: "pv".to_string(),
            id: Uuid::from_u128(0x1111),
            name: Some("s".to_string()),
            state: 1,
            decoder_info: None,
            stereo: true,
            line_id: Uuid::from_u128(0x2222),
            icon_visible: false,
            angle: 90,
        };
        let info = SourceInfo::Player(PlayerSourceInfo {
            player_info: VoicePlayerInfo {
                player_id: Uuid::from_u128(0x3333),
                player_nick: "n".to_string(),
                muted: false,
                voice_disabled: false,
                microphone_muted: true,
            },
            base,
        });
        info.serialize(&mut w).unwrap();
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(SourceInfo::deserialize(&mut r).unwrap(), info);
        assert!(r.is_empty());
    }

    #[test]
    fn source_info_direct_roundtrip() {
        let mut w = pw();
        let base = SourceInfoBase {
            addon_id: "pv".to_string(),
            id: Uuid::from_u128(0x4444),
            name: None,
            state: 0,
            decoder_info: Some(CodecInfo {
                name: "opus".to_string(),
                params: vec![],
            }),
            stereo: false,
            line_id: Uuid::from_u128(0x5555),
            icon_visible: true,
            angle: 0,
        };
        let info = SourceInfo::Direct(DirectSourceInfo {
            sender: None,
            relative_position: Some(Pos3d::new(1.0, 2.0, 3.0)),
            camera_relative: true,
            base,
            look_angle: Pos3d::zero(),
        });
        info.serialize(&mut w).unwrap();
        let mut r = WireReader::new(w.as_slice());
        let back = SourceInfo::deserialize(&mut r).unwrap();
        assert!(r.is_empty());
        assert_eq!(back, info);
    }

    #[test]
    fn source_info_unknown_type() {
        let mut w = pw();
        w.write_utf("ALIEN").unwrap();
        let mut r = WireReader::new(w.as_slice());
        assert!(matches!(
            SourceInfo::deserialize(&mut r),
            Err(VoiceError::UnknownEnumName("SourceInfo.Type", _))
        ));
    }

    #[test]
    fn self_source_info_roundtrip() {
        let mut w = pw();
        let base = SourceInfoBase {
            addon_id: "a".to_string(),
            id: Uuid::from_u128(0x6666),
            name: None,
            state: 0,
            decoder_info: None,
            stereo: false,
            line_id: Uuid::from_u128(0x7777),
            icon_visible: false,
            angle: 0,
        };
        let info = SelfSourceInfo {
            source_info: SourceInfo::Entity(EntitySourceInfo {
                base,
                entity_id: 42,
            }),
            player_id: Uuid::from_u128(0x8888),
            activation_id: Uuid::from_u128(0x9999),
            sequence_number: 12345,
        };
        info.serialize(&mut w).unwrap();
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(SelfSourceInfo::deserialize(&mut r).unwrap(), info);
    }
}