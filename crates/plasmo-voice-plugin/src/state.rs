//! Voice server state that survives a plugin reload.
//!
//! Everything here is plain Rust with no WASI/WIT dependency so it can be unit
//! tested on the host: the WIT glue in `lib.rs` only decides *where* the file
//! lives (`Context::get_data_folder()`, which the host maps to the guest path
//! `data`).

use std::collections::BTreeMap;

use plasmo_voice_core::PROTOCOL_VERSION;

/// File name inside the plugin data folder.
pub const STATE_FILE: &str = "state.json";

/// Persisted server state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceServerState {
    /// The UDP port the voice server listens on.
    pub port: u16,
    /// The secret UUID handed to clients in `ConnectionPacket`.
    ///
    /// Plasmo Voice uses a UUID-shaped secret for every client connection; the
    /// server keeps the authoritative one so a reload does not silently
    /// invalidate clients that are already connected.
    pub secret: String,
    /// Protocol version this server advertises (`2.1.7` upstream).
    pub protocol_version: String,
    /// Whether the UDP listener is enabled at all.
    pub enabled: bool,
}

impl Default for VoiceServerState {
    fn default() -> Self {
        Self {
            port: 0,
            secret: String::new(),
            protocol_version: PROTOCOL_VERSION.to_string(),
            enabled: true,
        }
    }
}

impl VoiceServerState {
    /// Parses the state from a JSON document.
    ///
    /// The parser is deliberately tiny and dependency-free: a flat object of
    /// string/number/bool values is all this file ever holds.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let mut state = Self::default();

        for (key, value) in parse_flat_object(text)? {
            match key.as_str() {
                "port" => {
                    state.port = value
                        .parse::<u16>()
                        .map_err(|e| format!("invalid port {value:?}: {e}"))?;
                }
                "secret" => state.secret = unquote(&value),
                "protocol_version" => state.protocol_version = unquote(&value),
                "enabled" => {
                    state.enabled = value
                        .parse::<bool>()
                        .map_err(|e| format!("invalid enabled {value:?}: {e}"))?;
                }
                // Unknown keys are ignored on purpose so a newer build can add
                // fields without breaking an older one.
                _ => {}
            }
        }

        Ok(state)
    }

    /// Serializes the state to a JSON document.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"port\": {},\n  \"secret\": \"{}\",\n  \"protocol_version\": \"{}\",\n  \"enabled\": {}\n}}\n",
            self.port,
            escape(&self.secret),
            escape(&self.protocol_version),
            self.enabled,
        )
    }

    /// Generates a fresh secret UUID-shaped string from `bytes`.
    ///
    /// Kept separate from the random source so tests can pass fixed bytes.
    #[must_use]
    pub fn secret_from_bytes(bytes: &[u8; 16]) -> String {
        let mut b = *bytes;
        // Set the version (4) and variant (RFC 4122) bits, like `Uuid::new_v4`.
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;

        let hex = |slice: &[u8]| slice.iter().map(|x| format!("{x:02x}")).collect::<String>();
        format!(
            "{}-{}-{}-{}-{}",
            hex(&b[0..4]),
            hex(&b[4..6]),
            hex(&b[6..8]),
            hex(&b[8..10]),
            hex(&b[10..16]),
        )
    }

    /// Returns the secret, generating (and returning) a fresh one when unset.
    ///
    /// The caller is expected to store the returned state so the generated
    /// secret is persisted.
    #[must_use]
    pub fn ensure_secret(&mut self, bytes: [u8; 16]) -> &str {
        if self.secret.is_empty() {
            self.secret = Self::secret_from_bytes(&bytes);
        }
        &self.secret
    }

    /// Reads `state.json` from `folder`, falling back to defaults.
    ///
    /// A corrupt or missing file is deliberately not fatal: the server starts with
    /// defaults and rewrites the file on the next save, so a bad edit cannot brick
    /// the plugin. The failure is logged at `warn` (corrupt) or `debug` (absent),
    /// because "first run" is not a problem worth shouting about.
    #[must_use]
    pub fn load(folder: &str) -> Self {
        let path = format!("{folder}/{STATE_FILE}");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_json(&text).unwrap_or_else(|error| {
                tracing::warn!(
                    %path,
                    %error,
                    "ignoring unreadable voice server state; using defaults"
                );
                Self::default()
            }),
            Err(error) => {
                tracing::debug!(%path, %error, "no voice server state yet; using defaults");
                Self::default()
            }
        }
    }

    /// Writes `state.json` into `folder`.
    ///
    /// # Errors
    ///
    /// Fails when the data folder is not writable, which the caller must surface:
    /// silently continuing would lose the secret that connected clients are using.
    pub fn save(&self, folder: &str) -> Result<(), String> {
        let path = format!("{folder}/{STATE_FILE}");
        std::fs::write(&path, self.to_json())
            .map_err(|error| format!("could not write {path}: {error}"))
    }

    /// Derives a secret UUID from the platform's strongest available entropy.
    ///
    /// `wasm32-wasip2` has no `getrandom` backend wired up, so this mixes the wall
    /// clock with the bound port through xorshift64*. The result is not
    /// cryptographically strong, but the voice secret only needs to be
    /// unguessable-per-server and stable across restarts, and the worst case for a
    /// collision is two servers sharing an obfuscation key. The persisted state is
    /// what actually makes it stable.
    #[must_use]
    pub fn generate_secret(port: u16) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());

        let mut bytes = [0u8; 16];
        let mut mix = nanos as u64 ^ u64::from(port).rotate_left(17);
        // xorshift64*: cheap, dependency-free, and enough to spread the bits below.
        for chunk in bytes.chunks_mut(8) {
            mix ^= mix >> 12;
            mix ^= mix << 25;
            mix ^= mix >> 27;
            let value = mix.wrapping_mul(0x2545_F491_4F6C_DD1D);
            chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
        }
        Self::secret_from_bytes(&bytes)
    }
}

/// Minimal flat-JSON object parser: `{ "k": v, ... }` where `v` is a number,
/// bool, or quoted string. Returns `(key, raw_value)` pairs; quoted strings are
/// returned **with** their surrounding quotes so `unquote` can decide.
fn parse_flat_object(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;

    let skip_ws = |i: &mut usize| {
        while *i < bytes.len() && (bytes[*i] as char).is_whitespace() {
            *i += 1;
        }
    };

    skip_ws(&mut i);
    if i >= bytes.len() || bytes[i] != b'{' {
        return Err("expected '{'".to_string());
    }
    i += 1;

    loop {
        skip_ws(&mut i);
        if i < bytes.len() && bytes[i] == b'}' {
            return Ok(map);
        }
        if i >= bytes.len() {
            return Err("unterminated object".to_string());
        }

        // key
        if bytes[i] != b'"' {
            return Err("expected quoted key".to_string());
        }
        let key = read_string(bytes, &mut i)?;
        skip_ws(&mut i);
        if i >= bytes.len() || bytes[i] != b':' {
            return Err("expected ':'".to_string());
        }
        i += 1;
        skip_ws(&mut i);

        // value
        let value = if i < bytes.len() && bytes[i] == b'"' {
            let raw = read_string(bytes, &mut i)?;
            format!("\"{raw}\"")
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' && bytes[i] != b'}' {
                i += 1;
            }
            text[start..i].trim().to_string()
        };
        map.insert(key, value);

        skip_ws(&mut i);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
            continue;
        }
        if i < bytes.len() && bytes[i] == b'}' {
            return Ok(map);
        }
        return Err("expected ',' or '}'".to_string());
    }
}

/// Reads a quoted string starting at `*i` (which must be the opening quote),
/// handling `\"`, `\\`, `\n`, `\r`, `\t` and leaving `*i` after the closing
/// quote.
fn read_string(bytes: &[u8], i: &mut usize) -> Result<String, String> {
    *i += 1; // opening quote
    let mut out = String::new();
    while *i < bytes.len() {
        match bytes[*i] {
            b'"' => {
                *i += 1;
                return Ok(out);
            }
            b'\\' => {
                *i += 1;
                if *i >= bytes.len() {
                    break;
                }
                out.push(match bytes[*i] {
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    other => other as char,
                });
                *i += 1;
            }
            other => {
                out.push(other as char);
                *i += 1;
            }
        }
    }
    Err("unterminated string".to_string())
}

/// Strips the surrounding quotes from a raw JSON value, if present.
fn unquote(raw: &str) -> String {
    raw.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw)
        .to_string()
}

/// Escapes a string for embedding in JSON.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let state = VoiceServerState {
            port: 25565,
            secret: "8f14e45f-ceea-467a-9a2e-1b0e5c3a7d11".to_string(),
            protocol_version: PROTOCOL_VERSION.to_string(),
            enabled: false,
        };
        let json = state.to_json();
        let back = VoiceServerState::from_json(&json).expect("parse");
        assert_eq!(back, state);
    }

    #[test]
    fn parses_minimal_document_with_defaults() {
        let state = VoiceServerState::from_json("{}").expect("parse");
        assert_eq!(state, VoiceServerState::default());
        assert!(state.enabled);
        assert_eq!(state.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn ignores_unknown_keys_and_whitespace() {
        let json =
            "{\n  \"port\" : 12345 ,\n  \"future_key\": \"ignored\",\n  \"enabled\": true\n}";
        let state = VoiceServerState::from_json(json).expect("parse");
        assert_eq!(state.port, 12345);
    }

    #[test]
    fn rejects_malformed_documents() {
        assert!(VoiceServerState::from_json("not json").is_err());
        assert!(VoiceServerState::from_json("{ \"port\": }").is_err());
        assert!(VoiceServerState::from_json("{ \"port\": \"abc\" }").is_err());
        assert!(VoiceServerState::from_json("{ \"enabled\": maybe }").is_err());
    }

    #[test]
    fn secret_is_v4_shaped_and_stable() {
        let mut state = VoiceServerState::default();
        let secret = state.ensure_secret([0u8; 16]).to_string();
        // version nibble 4, variant nibble 8..b
        assert_eq!(&secret[14..15], "4");
        assert!(matches!(&secret[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(secret.len(), 36);

        // Second call must not regenerate.
        let again = state.ensure_secret([0xff; 16]).to_string();
        assert_eq!(secret, again);
    }

    #[test]
    fn escapes_and_unescapes_strings() {
        let [state] = [VoiceServerState {
            secret: "a\"b\\c\nd".to_string(),
            ..VoiceServerState::default()
        }];
        let back = VoiceServerState::from_json(&state.to_json()).expect("parse");
        assert_eq!(back.secret, state.secret);
    }
}
