//! Minimal, dependency-free MD5 implementation used to replicate
//! `UUID.nameUUIDFromBytes(...)` — the way Plasmo Voice derives activation and
//! source-line ids. Only used for those ids; no crypto guarantees intended.
//!
//! Test vectors are verified against RFC 1321 ("abc" -> 90015098...) and the
//! empty message.

/// Raw MD5 digest over `data`.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];

    // Pre-computed constants.
    let k: [u32; 64] = {
        let mut k = [0u32; 64];
        for (i, item) in k.iter_mut().enumerate() {
            *item = ((i as f64 + 1.0).sin().abs() * (1u64 << 32) as f64) as u32;
        }
        k
    };

    // Per-round shift amounts (RFC 1321 table).
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
        5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }

        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);

        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(k[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = tmp;
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut out = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// `UUID.nameUUIDFromBytes(name)`: MD5 over `name` (and nothing else, per the
/// real Plasmo Voice code — unlike RFC 4122 there is **no** namespace prefix),
/// then version (0x30) and variant (0x80) bits are set on the raw digest.
pub fn name_uuid_from_bytes(name: &[u8]) -> uuid::Uuid {
    let mut h = md5(name);
    h[6] = (h[6] & 0x0F) | 0x30; // version 3
    h[8] = (h[8] & 0x3F) | 0x80; // RFC 4122 variant
    uuid::Uuid::from_slice(&h).expect("16 bytes are always a valid UUID")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn rfc1321_empty() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn rfc1321_abc() {
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn rfc1321_message_digest() {
        assert_eq!(
            hex(&md5(b"message digest")),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
    }

    #[test]
    fn rfc1321_long() {
        assert_eq!(
            hex(&md5(b"abcdefghijklmnopqrstuvwxyz")),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
    }

    #[test]
    fn name_uuid_reproducible() {
        // Recompute the same value twice and check version/variant bits.
        let a = name_uuid_from_bytes(b"proximity_activation");
        let b = name_uuid_from_bytes(b"proximity_activation");
        assert_eq!(a, b);
        assert_eq!(a.get_version(), Some(uuid::Version::Md5));
        assert_eq!(a.get_variant(), uuid::Variant::RFC4122);
        // Independently checked against the JVM:
        // nameUUIDFromBytes("proximity_activation") == 4aec07ba-d109-3345-9a0a-92022a116cd0
        assert_eq!(
            a.to_string(),
            "4aec07ba-d109-3345-9a0a-92022a116cd0"
        );
    }
}
