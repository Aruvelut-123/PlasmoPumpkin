//! Big-endian read/write primitives over a byte buffer, matching Guava's
//! `ByteArrayDataInput`/`ByteArrayDataOutput` semantics used by Plasmo Voice,
//! including Java's modified UTF-8 string encoding.

use uuid::Uuid;

use crate::error::{Result, VoiceError};

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Big-endian writer over an owned byte buffer (analogue of
/// `ByteArrayDataOutput`). Strings use modified UTF-8 like `DataOutput.writeUTF`.
#[derive(Debug, Default, Clone)]
pub struct WireWriter {
    buf: Vec<u8>,
}

impl WireWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Returns the written bytes.
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns the written bytes without consuming the writer.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn write_i8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }

    pub fn write_bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    pub fn write_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
    }

    pub fn write_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// `PacketUtil.writeUUID`: 16 raw bytes, big-endian msb/lsb.
    pub fn write_uuid(&mut self, id: Uuid) {
        self.buf.extend_from_slice(id.as_bytes());
    }

    /// `PacketUtil.writeNullableString`: bool present + modified UTF-8.
    pub fn write_optional_string(&mut self, s: Option<&str>) -> Result<()> {
        self.write_bool(s.is_some());
        if let Some(s) = s {
            self.write_utf(s)?;
        }
        Ok(())
    }

    /// Writes a string using Java modified UTF-8 (`DataOutput.writeUTF`):
    /// a 16-bit big-endian byte-length prefix followed by the encoded bytes.
    ///
    /// - U+0000 encodes as `C0 80`
    /// - U+0001..=U+007F as 1 byte
    /// - U+0080..=U+07FF as 2 bytes
    /// - U+0800..=U+FFFF as 3 bytes
    /// - astral code points as a 6-byte surrogate pair (3 bytes per half)
    pub fn write_utf(&mut self, s: &str) -> Result<()> {
        let mut encoded_len = 0usize;
        for ch in s.chars() {
            encoded_len += modified_utf8_len(ch);
        }
        if encoded_len > 0xFFFF {
            return Err(VoiceError::StringTooLong { len: encoded_len });
        }

        self.write_u16(encoded_len as u16);
        for ch in s.chars() {
            write_modified_char(&mut self.buf, ch);
        }
        Ok(())
    }
}

/// Number of modified-UTF-8 bytes for a code point.
fn modified_utf8_len(ch: char) -> usize {
    let cp = ch as u32;
    match cp {
        0x0000 => 2,
        0x0001..=0x007F => 1,
        0x0080..=0x07FF => 2,
        0x0800..=0xFFFF => 3,
        _ => 6, // astral: surrogate pair, 3 bytes each
    }
}

/// Appends the modified UTF-8 encoding of one code point to `buf`.
fn write_modified_char(buf: &mut Vec<u8>, ch: char) {
    let cp = ch as u32;
    if cp <= 0x007F {
        if cp == 0x0000 {
            buf.extend_from_slice(&[0xC0, 0x80]);
        } else {
            buf.push(cp as u8);
        }
    } else if cp <= 0x07FF {
        buf.extend_from_slice(&[0xC0 | (cp >> 6) as u8, 0x80 | (cp & 0x3F) as u8]);
    } else if cp <= 0xFFFF {
        buf.extend_from_slice(&[
            0xE0 | (cp >> 12) as u8,
            0x80 | ((cp >> 6) & 0x3F) as u8,
            0x80 | (cp & 0x3F) as u8,
        ]);
    } else {
        // Astral code point -> UTF-16 surrogate pair, each half encoded in 3 bytes.
        let v = cp - 0x10000;
        let high = 0xD800 + (v >> 10);
        let low = 0xDC00 + (v & 0x3FF);
        write_surrogate_half(buf, high);
        write_surrogate_half(buf, low);
    }
}

/// Encodes one UTF-16 code unit (which may be a surrogate half) in 3 bytes.
fn write_surrogate_half(buf: &mut Vec<u8>, u: u32) {
    buf.extend_from_slice(&[
        0xE0 | (u >> 12) as u8,
        0x80 | ((u >> 6) & 0x3F) as u8,
        0x80 | (u & 0x3F) as u8,
    ]);
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Big-endian reader over a borrowed byte slice (analogue of
/// `ByteArrayDataInput`). Reads past the end yield `VoiceError::UnexpectedEof`.
#[derive(Debug, Clone)]
pub struct WireReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) -> Result<()> {
        if pos > self.data.len() {
            return Err(VoiceError::UnexpectedEof {
                needed: pos,
                remaining: self.data.len(),
            });
        }
        self.pos = pos;
        Ok(())
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Bytes that have not been consumed yet.
    pub fn tail(&self) -> &'a [u8] {
        &self.data[self.pos.min(self.data.len())..]
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(VoiceError::UnexpectedEof {
            needed: n,
            remaining: self.data.len().saturating_sub(self.pos),
        })?;
        if end > self.data.len() {
            return Err(VoiceError::UnexpectedEof {
                needed: n,
                remaining: self.data.len().saturating_sub(self.pos),
            });
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn read_i8(&mut self) -> Result<i8> {
        Ok(self.take(1)?[0] as i8)
    }

    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_u16()? as i16)
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(self.read_u32()? as i32)
    }

    pub fn read_u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        Ok(self.read_u64()? as i64)
    }

    pub fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.read_u32()?))
    }

    pub fn read_f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.read_u64()?))
    }

    /// `PacketUtil.readUUID`: 16 raw bytes.
    pub fn read_uuid(&mut self) -> Result<Uuid> {
        let b = self.take(16)?;
        Uuid::from_slice(b).map_err(|_| VoiceError::InvalidState("bad uuid bytes"))
    }

    /// `PacketUtil.readNullableString`: bool present + UTF-8 string.
    pub fn read_optional_string(&mut self) -> Result<Option<String>> {
        Ok(if self.read_bool()? {
            Some(self.read_utf()?)
        } else {
            None
        })
    }

    /// Reads `count` bytes into a newly allocated vec.
    pub fn read_bytes(&mut self, count: usize) -> Result<Vec<u8>> {
        Ok(self.take(count)?.to_vec())
    }

    /// Reads a Java modified UTF-8 string (16-bit length prefix).
    ///
    /// Decodes like `DataInputStream.readUTF`: 1/2/3-byte sequences, combining
    /// 3-byte surrogate pairs back into astral code points. Unpaired
    /// surrogates (which Java tolerates but Rust does not) are replaced with
    /// U+FFFD so decoding never panics.
    pub fn read_utf(&mut self) -> Result<String> {
        let len = self.read_u16()? as usize;
        let bytes = self.take(len)?;
        decode_modified_utf8(bytes)
    }

    /// `PacketUtil.readSafeUTF`: read a string and enforce a max length in
    /// characters (Java `String.length()`, approximated with `chars().count()`).
    pub fn read_safe_utf(&mut self, max_len: usize) -> Result<String> {
        let s = self.read_utf()?;
        if s.chars().count() > max_len {
            return Err(VoiceError::StringLimitExceeded { max: max_len });
        }
        Ok(s)
    }

    /// `PacketUtil.readSafeInt`: read an int and enforce `[min, max]`.
    pub fn read_safe_int(&mut self, min: i32, max: i32) -> Result<i32> {
        let v = self.read_i32()?;
        if v < min || v > max {
            return Err(VoiceError::OutOfBoundsInt { value: v, min, max });
        }
        Ok(v)
    }
}

/// Decodes modified UTF-8 bytes into a Rust `String`.
///
/// Rules mirror `DataInputStream.readUTF`:
/// - `0xxxxxxx` -> 1 byte, value as-is
/// - `110xxxxx 10xxxxxx` -> 2 bytes, must not be overlong (< 0x80)
/// - `1110xxxx 10xxxxxx 10xxxxxx` -> 3 bytes, must not be overlong (< 0x800)
/// - A high surrogate (0xD800..=0xDBFF) followed by a low surrogate
///   (0xDC00..=0xDFFF), each as 3-byte sequences, combines into an astral code
///   point.
/// - Any other byte pattern is malformed.
fn decode_modified_utf8(bytes: &[u8]) -> Result<String> {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0usize;
    let n = bytes.len();

    while i < n {
        let b0 = bytes[i];
        match b0 >> 4 {
            0..=7 => {
                // 0xxxxxxx
                out.push(b0 as char);
                i += 1;
            }
            12 | 13 => {
                // 110x xxxx 10xx xxxx
                if i + 2 > n {
                    return Err(VoiceError::MalformedUtf8);
                }
                let b1 = bytes[i + 1];
                if (b1 & 0xC0) != 0x80 {
                    return Err(VoiceError::MalformedUtf8);
                }
                let c = (((b0 & 0x1F) as u32) << 6) | ((b1 & 0x3F) as u32);
                // 0xC0 0x80 is Java writeUTF's encoding of U+0000: accept it.
                // Any other 2-byte form decoding below 0x80 is an overlong
                // sequence Java never produces, so reject it.
                if c == 0 {
                    out.push('\0');
                    i += 2;
                    continue;
                }
                if c < 0x80 {
                    return Err(VoiceError::MalformedUtf8);
                }
                out.push(char::from_u32(c).ok_or(VoiceError::MalformedUtf8)?);
                i += 2;
            }
            14 => {
                // 1110 xxxx 10xx xxxx 10xx xxxx
                if i + 3 > n {
                    return Err(VoiceError::MalformedUtf8);
                }
                let b1 = bytes[i + 1];
                let b2 = bytes[i + 2];
                if (b1 & 0xC0) != 0x80 || (b2 & 0xC0) != 0x80 {
                    return Err(VoiceError::MalformedUtf8);
                }
                let c = (((b0 & 0x0F) as u32) << 12)
                    | (((b1 & 0x3F) as u32) << 6)
                    | ((b2 & 0x3F) as u32);
                if c < 0x800 {
                    return Err(VoiceError::MalformedUtf8);
                }
                if (0xD800..=0xDFFF).contains(&c) {
                    // Surrogate half: try to combine a pair.
                    if c <= 0xDBFF && i + 6 <= n {
                        // Peek the next 3-byte sequence.
                        let n0 = bytes[i + 3];
                        if (n0 >> 4) == 14 {
                            let n1 = bytes[i + 4];
                            let n2 = bytes[i + 5];
                            if (n1 & 0xC0) == 0x80 && (n2 & 0xC0) == 0x80 {
                                let low = (((n0 & 0x0F) as u32) << 12)
                                    | (((n1 & 0x3F) as u32) << 6)
                                    | ((n2 & 0x3F) as u32);
                                if (0xDC00..=0xDFFF).contains(&low) {
                                    let cp = 0x10000 + ((c - 0xD800) << 10) + (low - 0xDC00);
                                    if let Some(ch) = char::from_u32(cp) {
                                        out.push(ch);
                                        i += 6;
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    // Unpaired surrogate: Java keeps it, Rust cannot.
                    out.push('\u{FFFD}');
                } else if let Some(ch) = char::from_u32(c) {
                    out.push(ch);
                } else {
                    return Err(VoiceError::MalformedUtf8);
                }
                i += 3;
            }
            _ => return Err(VoiceError::MalformedUtf8),
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_ascii_roundtrip() {
        let mut w = WireWriter::new();
        w.write_utf("hello").unwrap();
        let bytes = w.into_inner();
        assert_eq!(bytes, [0x00, 0x05, b'h', b'e', b'l', b'l', b'o']);
        let mut r = WireReader::new(&bytes);
        assert_eq!(r.read_utf().unwrap(), "hello");
    }

    #[test]
    fn utf8_null_character() {
        let mut w = WireWriter::new();
        w.write_utf("\u{0000}").unwrap();
        let bytes = w.into_inner();
        // Java encodes U+0000 as C0 80 (2 bytes).
        assert_eq!(bytes, [0x00, 0x02, 0xC0, 0x80]);
        let mut r = WireReader::new(&bytes);
        assert_eq!(r.read_utf().unwrap(), "\u{0000}");
    }

    #[test]
    fn utf8_multibyte_roundtrip() {
        let s = "héllo wörld 中文 🎃👻";
        let mut w = WireWriter::new();
        w.write_utf(s).unwrap();
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(r.read_utf().unwrap(), s);
    }

    #[test]
    fn utf8_surrogate_pair_length() {
        // 🎃 is U+1F383: 6 bytes in modified UTF-8.
        let mut w = WireWriter::new();
        w.write_utf("🎃").unwrap();
        let bytes = w.into_inner();
        assert_eq!(&bytes[0..2], &[0x00, 0x06]);
        // D83C DF83 pair: ED A0 BC ED BE 83
        assert_eq!(&bytes[2..], &[0xED, 0xA0, 0xBC, 0xED, 0xBE, 0x83]);
        let mut r = WireReader::new(&bytes);
        assert_eq!(r.read_utf().unwrap(), "🎃");
    }

    #[test]
    fn utf8_overlong_rejected() {
        // C1 80 would encode 0x40 as a 2-byte sequence: an overlong form Java
        // writeUTF never produces, so it must be rejected. (0xC0 0x80 *is*
        // Java's NUL encoding and is accepted — see utf8_null_character.)
        let mut r = WireReader::new(&[0x00, 0x02, 0xC1, 0x80]);
        assert!(r.read_utf().is_err());
    }

    #[test]
    fn utf8_length_limit() {
        let long = "x".repeat(65_536);
        let mut w = WireWriter::new();
        assert!(matches!(
            w.write_utf(&long),
            Err(VoiceError::StringTooLong { .. })
        ));
    }

    #[test]
    fn primitives_big_endian() {
        let mut w = WireWriter::new();
        w.write_i32(-12345);
        w.write_u64(0x0102_0304_0506_0708);
        w.write_f32(1.5);
        w.write_f64(-2.25);
        w.write_uuid(Uuid::from_u128(0x00112233445566778899aabbccddeeff));
        let bytes = w.into_inner();
        let mut r = WireReader::new(&bytes);
        assert_eq!(r.read_i32().unwrap(), -12345);
        assert_eq!(r.read_u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.read_f32().unwrap(), 1.5);
        assert_eq!(r.read_f64().unwrap(), -2.25);
        assert_eq!(
            r.read_uuid().unwrap(),
            Uuid::from_u128(0x00112233445566778899aabbccddeeff)
        );
        assert!(r.is_empty());
    }

    #[test]
    fn eof_error() {
        let mut r = WireReader::new(&[0x01]);
        assert!(matches!(
            r.read_u32(),
            Err(VoiceError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn optional_string_roundtrip() {
        let mut w = WireWriter::new();
        w.write_optional_string(Some("abc")).unwrap();
        w.write_optional_string(None).unwrap();
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(r.read_optional_string().unwrap(), Some("abc".to_string()));
        assert_eq!(r.read_optional_string().unwrap(), None);
    }
}
