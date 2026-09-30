//! Error types shared across the protocol core.

use std::fmt;

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, VoiceError>;

/// All errors that can surface while encoding/decoding protocol data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceError {
    /// A packet id that is not registered (or out of the known range).
    UnknownPacketId(u32),
    /// A packet id that is not valid for the given direction.
    WrongDirection(u32),
    /// An enum tag read from the wire is not a known variant.
    UnknownEnumValue(&'static str, i32),
    /// A dynamic enum tag (e.g. `SourceInfo.Type.valueOf(...)`) is unknown.
    UnknownEnumName(&'static str, String),
    /// A string read from the wire is not valid modified-UTF-8.
    InvalidUtf8,
    /// Writing a string whose modified-UTF-8 encoding exceeds 65535 bytes.
    StringTooLong { len: usize },
    /// A string read from the wire exceeds the allowed character limit.
    StringLimitExceeded { max: usize },
    /// An int read from the wire is outside the allowed range.
    OutOfBoundsInt { value: i32, min: i32, max: i32 },
    /// The reader ran out of bytes while reading a field.
    UnexpectedEof { needed: usize, remaining: usize },
    /// Leftover bytes after a packet body was fully read.
    TrailingBytes(usize),
    /// A value that must not be null on the wire was null.
    NullValue,
    /// Internal invariant violation (bad UUID bytes, ...).
    InvalidState(&'static str),
    /// Malformed modified-UTF-8 byte sequence.
    MalformedUtf8,
    /// Payload checksum mismatch (UDP packet).
    ChecksumMismatch { expected: u32, actual: u32 },
    /// AES block cipher failed (UDP payload decrypt).
    CipherError(&'static str),
    /// The UDP magic number is not `0x4e9004e9`.
    BadMagic(u32),
    /// The packet is shorter than the fixed header.
    PacketTooShort { min: usize, actual: usize },
}

impl fmt::Display for VoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VoiceError::UnknownPacketId(id) => write!(f, "unknown packet id {id}"),
            VoiceError::WrongDirection(id) => write!(f, "packet id {id} not valid for direction"),
            VoiceError::UnknownEnumValue(name, v) => write!(f, "unknown {name} value {v}"),
            VoiceError::UnknownEnumName(name, tag) => write!(f, "unknown {name} tag \"{tag}\""),
            VoiceError::InvalidUtf8 => write!(f, "invalid modified utf-8 string"),
            VoiceError::StringTooLong { len } => {
                write!(f, "string too long to encode ({len} bytes > 65535)")
            }
            VoiceError::StringLimitExceeded { max } => {
                write!(f, "string exceeds max length ({max} chars)")
            }
            VoiceError::OutOfBoundsInt { value, min, max } => {
                write!(f, "int value {value} out of range [{min}, {max}]")
            }
            VoiceError::UnexpectedEof { needed, remaining } => {
                write!(
                    f,
                    "unexpected end of data (needed {needed}, had {remaining})"
                )
            }
            VoiceError::TrailingBytes(n) => write!(f, "trailing {n} bytes after packet body"),
            VoiceError::NullValue => write!(f, "unexpected null value on the wire"),
            VoiceError::InvalidState(what) => write!(f, "invalid state: {what}"),
            VoiceError::MalformedUtf8 => write!(f, "malformed modified utf-8"),
            VoiceError::ChecksumMismatch { expected, actual } => {
                write!(f, "checksum mismatch (expected {expected}, got {actual})")
            }
            VoiceError::CipherError(what) => write!(f, "cipher error: {what}"),
            VoiceError::BadMagic(magic) => write!(f, "bad udp magic 0x{magic:08x}"),
            VoiceError::PacketTooShort { min, actual } => {
                write!(f, "packet too short (need {min} bytes, got {actual})")
            }
        }
    }
}

impl std::error::Error for VoiceError {}
