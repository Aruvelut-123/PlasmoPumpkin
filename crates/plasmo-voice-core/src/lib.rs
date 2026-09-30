//! Pure-Rust wire-format core for the Plasmo Voice protocol.
//!
//! This crate reimplements `su.plo.voice.proto` (Plasmo Voice 2.x protocol)
//! byte-for-byte: UDP and TCP packet codecs, packet types, and the serializable
//! data models (activations, source lines, source info, player info, ...).
//!
//! Design notes:
//! - Multi-byte integers, longs and floats are big-endian, exactly like Guava's
//!   `ByteArrayDataInput`/`ByteArrayDataOutput` on the JVM.
//! - Strings use the *modified UTF-8* encoding of `DataOutput.writeUTF`, so
//!   0x0000 encodes as 2 bytes and astral code points as 6 bytes (surrogate
//!   pairs). The length prefix is an unsigned big-endian 16-bit value.
//! - `VoiceActivation::generate_id` and `VoiceSourceLine::generate_id` replicate
//!   `UUID.nameUUIDFromBytes` (MD5 with version/variant bits set) — note that
//!   Plasmo Voice calls it on `name + "_activation"` / `name + "_line"`
//!   **without** the UUID namespace, unlike RFC 4122's v3.

pub mod data;
pub mod error;
pub mod md5;
pub mod util;
pub mod wire;

pub use error::VoiceError;
pub use util::{WireReader, WireWriter};
pub use wire::{
    ConnectionPacket, PacketDirection, PacketRegistry, SourceInfoPacket, TcpCodec, TcpPacket,
    UdpCodec, UdpEnvelope, UdpPacket,
};

pub const PROTOCOL_VERSION: &str = "2.1.7";
