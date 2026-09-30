//! The key exchange behind `ConfigPacket.encryption`.
//!
//! Upstream sends `EncryptionInfo("AES/CBC/PKCS5Padding", RSA(clientPublicKey, aesKey))`
//! (`VoiceTcpServerConnectionManager.java:110-125`): the *plaintext* is the server-wide
//! 16-byte AES key, and the algorithm name tells the client which cipher to run on its own
//! audio.
//!
//! The server is never a party to that cipher. `NettyPacketHandler` relays a
//! `PlayerAudioPacket`'s `data` byte-for-byte into a `SourceAudioPacket` — the frame was
//! already encrypted by the client that recorded it, with the key delivered here — so a
//! relay needs **RSA public-key encryption and nothing else**. There is deliberately no AES
//! implementation in this crate: nothing would ever call it.
//!
//! Two details are wire-visible and must not drift:
//!
//! * the algorithm string is passed to `Cipher.getInstance` by the client, so it is
//!   [`AES_ALGORITHM`] verbatim;
//! * the padding is `RSA/ECB/PKCS1Padding` — Java's `Cipher.getInstance("RSA")` default —
//!   which is RSAES-PKCS1-v1_5 ([`Pkcs1v15Encrypt`]), *not* OAEP.

use std::fmt;

use plasmo_voice_core::data::EncryptionInfo;
use rand_core::{OsRng, RngCore};
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};

/// `EncryptionInfo.algorithm`, exactly as upstream spells it.
///
/// The client feeds this string to `Cipher.getInstance`, so a typo is not a cosmetic bug:
/// an unknown transformation throws and the client tears its UDP connection down.
pub const AES_ALGORITHM: &str = "AES/CBC/PKCS5Padding";

/// The AES key length upstream uses (AES-128).
///
/// `BaseVoiceServer.aesEncryptionKey()` is a `byte[16]`, regenerated from a random UUID
/// when the config is created and persisted thereafter, so that every client in a session
/// shares one key.
pub const AES_KEY_LEN: usize = 16;

/// The server-wide AES key, in the one form this crate needs: something to RSA-encrypt.
///
/// It is compared and cloned but **never printed** — [`fmt::Debug`] is hand-written to
/// redact the bytes, because this value ends up inside `tracing` calls whose output lands
/// in a server log file.
#[derive(Clone, PartialEq, Eq)]
pub struct AesKey([u8; AES_KEY_LEN]);

impl fmt::Debug for AesKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AesKey(redacted)")
    }
}

impl AesKey {
    /// Wraps raw key bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; AES_KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Draws a fresh key from the host's entropy.
    ///
    /// # Errors
    ///
    /// Fails when no secure randomness is available. That must not be papered over with a
    /// weaker source: a guessable key is worse than the plaintext it would replace, because
    /// it makes the traffic *look* protected.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; AES_KEY_LEN];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|error| format!("the host provided no secure randomness: {error}"))?;
        Ok(Self(bytes))
    }

    /// Lowercase hex, for `state.json`.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(AES_KEY_LEN * 2);
        for byte in self.0 {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        out
    }

    /// Parses the lowercase or uppercase hex written by [`Self::to_hex`].
    ///
    /// # Errors
    ///
    /// Rejects anything that is not exactly `AES_KEY_LEN` bytes of hex, so a truncated
    /// state file cannot silently shorten the key.
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let digits = text.as_bytes();
        if digits.len() != AES_KEY_LEN * 2 {
            return Err(format!(
                "expected {} hex digits, got {}",
                AES_KEY_LEN * 2,
                digits.len()
            ));
        }

        let mut bytes = [0u8; AES_KEY_LEN];
        for (index, pair) in digits.as_chunks::<2>().0.iter().enumerate() {
            let high = hex_value(pair[0])
                .ok_or_else(|| format!("{:?} is not a hex digit", char::from(pair[0])))?;
            let low = hex_value(pair[1])
                .ok_or_else(|| format!("{:?} is not a hex digit", char::from(pair[1])))?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    /// Encrypts this key with the client's public key, for `ConfigPacket.encryption`.
    ///
    /// # Errors
    ///
    /// Fails when `public_key_der` is not an X.509 `SubjectPublicKeyInfo` RSA key — the
    /// encoding a Java client sends, because it reads the bytes out of
    /// `X509EncodedKeySpec(publicKey.getEncoded())` — or when the key is too small to hold
    /// a PKCS#1 v1.5 block.
    pub fn encrypt_for(&self, public_key_der: &[u8]) -> Result<Vec<u8>, String> {
        let public_key = RsaPublicKey::from_public_key_der(public_key_der).map_err(|error| {
            format!("the client's public key is not an X.509 RSA public key: {error}")
        })?;

        public_key
            .encrypt(&mut OsRng, Pkcs1v15Encrypt, &self.0)
            .map_err(|error| format!("RSA/PKCS1 encryption of the AES key failed: {error}"))
    }

    /// The `EncryptionInfo` a clientbound `ConfigPacket` carries.
    ///
    /// # Errors
    ///
    /// Same as [`Self::encrypt_for`].
    pub fn encryption_info(&self, public_key_der: &[u8]) -> Result<EncryptionInfo, String> {
        Ok(EncryptionInfo {
            algorithm: AES_ALGORITHM.to_string(),
            data: self.encrypt_for(public_key_der)?,
        })
    }
}

/// Lowercase hex digits, indexed by nibble value.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// The value of one hex digit, or `None` when the byte is not one.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePublicKey;

    use super::*;

    /// A 2048-bit key pair, generated once for the whole test binary.
    ///
    /// Generating this per test would dominate the suite's runtime, and the only property
    /// the tests need is that the client half is a real key.
    fn client_keypair() -> &'static (RsaPrivateKey, Vec<u8>) {
        static PAIR: OnceLock<(RsaPrivateKey, Vec<u8>)> = OnceLock::new();
        PAIR.get_or_init(|| {
            let private = RsaPrivateKey::new(&mut OsRng, 2048).expect("keygen");
            let der = RsaPublicKey::from(&private)
                .to_public_key_der()
                .expect("encode");
            (private, der.as_bytes().to_vec())
        })
    }

    #[test]
    fn a_generated_key_round_trips_through_hex() {
        let key = AesKey::generate().expect("entropy");
        let hex = key.to_hex();
        assert_eq!(hex.len(), AES_KEY_LEN * 2);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(AesKey::from_hex(&hex).expect("parse"), key);
        // Uppercase is accepted too; the state file is hand-editable.
        assert_eq!(AesKey::from_hex(&hex.to_uppercase()).expect("parse"), key);
    }

    #[test]
    fn two_generated_keys_differ() {
        let first = AesKey::generate().expect("entropy");
        let second = AesKey::generate().expect("entropy");
        assert_ne!(first, second, "the host entropy must not be a constant");
    }

    #[test]
    fn the_key_never_reaches_a_log_line() {
        let key = AesKey::from_bytes([0xab; AES_KEY_LEN]);
        let rendered = format!("{key:?}");
        assert_eq!(rendered, "AesKey(redacted)");
        assert!(!rendered.contains("ab"));
    }

    #[test]
    fn from_hex_rejects_anything_that_is_not_a_whole_key() {
        // Too short, too long, odd length, and a non-hex byte.
        assert!(AesKey::from_hex("00").is_err());
        assert!(AesKey::from_hex(&"00".repeat(17)).is_err());
        assert!(AesKey::from_hex(&"0".repeat(31)).is_err());
        assert!(AesKey::from_hex(&format!("{}zz", "0".repeat(30))).is_err());
        assert!(AesKey::from_hex("").is_err());
    }

    /// The property that matters: what this crate produces is what a Java client can open
    /// with `Cipher.getInstance("RSA")` in `DECRYPT_MODE`. PKCS#1 v1.5 is a standard, but
    /// the round trip is what proves *this* implementation is the right one.
    #[test]
    fn the_client_can_recover_the_key_from_the_wire_bytes() {
        let key = AesKey::from_bytes([0x5a; AES_KEY_LEN]);
        let (private, der) = client_keypair();

        let info = key.encryption_info(der).expect("encrypt");
        assert_eq!(info.algorithm, "AES/CBC/PKCS5Padding");
        assert_eq!(
            info.data.len(),
            256,
            "a PKCS#1 v1.5 block is exactly the modulus size of a 2048-bit key"
        );
        assert_ne!(
            info.data,
            key.to_hex().into_bytes(),
            "the wire must carry ciphertext, not the key"
        );

        let recovered = private
            .decrypt(Pkcs1v15Encrypt, &info.data)
            .expect("the client side decrypts");
        assert_eq!(recovered, [0x5a; AES_KEY_LEN]);
    }

    #[test]
    fn the_same_key_encrypts_differently_every_time() {
        // Fresh PKCS#1 v1.5 padding per call: two ciphertexts for one key must not match,
        // or the block would be deterministic and reveal a reused key to an observer.
        let key = AesKey::generate().expect("entropy");
        let (_, der) = client_keypair();
        let first = key.encrypt_for(der).expect("encrypt");
        let second = key.encrypt_for(der).expect("encrypt");
        assert_ne!(first, second);
    }

    #[test]
    fn a_public_key_that_is_not_rsa_is_refused() {
        let key = AesKey::generate().expect("entropy");
        assert!(key.encrypt_for(&[]).is_err());
        assert!(key.encrypt_for(&[0x30, 0x82, 0x01, 0x22]).is_err());
        // A DER blob that is not a public key at all.
        assert!(key.encrypt_for(&[0xff; 64]).is_err());
        // The shape the plugin's own control-plane test used before encryption existed.
        assert!(
            key.encrypt_for(&[0x30, 0x82, 0x01, 0x22, 0x00, 0x01, 0x02, 0x03])
                .is_err()
        );
    }
}
