use std::io::{self, Read, Write};
use bytes::{Buf, BufMut, BytesMut};
use uuid::Uuid;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum VoiceError {
    #[error("Magic number mismatch")]
    MagicMismatch,
    #[error("Unknown packet type {0}")]
    UnknownType(u8),
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
}

pub const MAGIC: u32 = 0x4e9004e9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Ping = 1,
    PlayerAudio = 2,
    SourceAudio = 3,
    SelfAudioInfo = 4,
    Custom = 0x100,
}

impl PacketType {
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            1 => Some(PacketType::Ping),
            2 => Some(PacketType::PlayerAudio),
            3 => Some(PacketType::SourceAudio),
            4 => Some(PacketType::SelfAudioInfo),
            0x100 => Some(PacketType::Custom),
            _ => None,
        }
    }

    pub fn to_u16(self) -> u16 {
        self as u16
    }
}

pub struct UdpPacket {
    pub magic: u32,
    pub packet_type: PacketType,
    pub secret: Uuid,
    pub timestamp: i64,
    pub body: Vec<u8>,
}

impl UdpPacket {
    pub fn encode(&self, buf: &mut BytesMut) -> Result<(), VoiceError> {
        buf.put_u32(self.magic);
        buf.put_u8(self.packet_type.to_u16() as u8); // type byte (low 8 bits; 0x100 -> 0x00)
        buf.put_u64(self.secret.as_u64_pair().0);
        buf.put_u64(self.secret.as_u64_pair().1);
        buf.put_i64(self.timestamp);
        buf.put_slice(&self.body);
        Ok(())
    }

    pub fn decode(mut buf: BytesMut) -> Result<Self, VoiceError> {
        if buf.get_u32() != MAGIC {
            return Err(VoiceError::MagicMismatch);
        }
        let raw = buf.get_u8() as u16;
        let ty = PacketType::from_u16(raw).ok_or(VoiceError::UnknownType(raw as u8))?;
        let secret = Uuid::from_u64_pair(buf.get_u64(), buf.get_u64());
        let timestamp = buf.get_i64();
        let body = buf.to_vec();
        Ok(UdpPacket {
            magic: MAGIC,
            packet_type: ty,
            secret,
            timestamp,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let pkt = UdpPacket {
            magic: MAGIC,
            packet_type: PacketType::Ping,
            secret: Uuid::new_v4(),
            timestamp: 1234567890,
            body: b"hello voice".to_vec(),
        };
        let mut buf = BytesMut::new();
        pkt.encode(&mut buf).unwrap();
        let decoded = UdpPacket::decode(buf).unwrap();
        assert_eq!(decoded.secret, pkt.secret);
        assert_eq!(decoded.timestamp, pkt.timestamp);
        assert_eq!(decoded.packet_type, PacketType::Ping);
        assert_eq!(decoded.body, pkt.body);
    }

    #[test]
    fn magic_reject() {
        let buf = BytesMut::from(&[0u8, 0, 0, 0, 1][..]);
        assert!(UdpPacket::decode(buf).is_err());
    }
}