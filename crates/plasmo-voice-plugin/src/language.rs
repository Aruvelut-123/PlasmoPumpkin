//! The server's translation table, and how a client's locale maps onto it.
//!
//! A `LanguageRequestPacket` is answered with the **client** scope of the requested locale
//! (`VoiceServerLanguages.getClientLanguage` → `PlayerChannelHandler:203-213`). Upstream
//! keeps that table in `languages/list` plus one `languages/<name>.toml` per locale, both
//! inside its jar, and Crowdin fills the rest in at runtime.
//!
//! A guest cannot read files it was not handed, so the same two things are compiled in
//! here: [`LIST`] is upstream's `languages/list` (`readLanguagesList`) and the [`FILES`]
//! table is the `languages/` directory. The lookup reproduces `getLanguage`:
//!
//! * the requested name is lowercased before the lookup (`languageName?.lowercase()`), so
//!   Minecraft's `zh_cn` finds the file we ship as `zh_cn.toml`;
//! * a locale this server does not ship falls back to the default locale;
//! * every locale is merged over the default one (`fillMissing`), so a key a translation
//!   does not carry still resolves.
//!
//! Only the `client` scope matters here: it is the half a `LanguagePacket` carries, and
//! the reason this table is not cosmetic is `pv.activation.proximity` — the volume tab
//! renders `translatable(sourceLine.getTranslation())`, and the mod's own
//! `lang/en_us.json` does not define that key (see [`crate::config::PROXIMITY_TRANSLATION`]).

use std::collections::BTreeMap;

/// `VoiceServerLanguages.FALLBACK_LANGUAGE` — every lookup ends here.
pub const FALLBACK_LANGUAGE: &str = "en_us";

/// `languages/list`, one locale per line (`readLanguagesList`). The test below is what
/// holds it in step with [`FILES`]; the control plane logs its length.
const LIST: &str = include_str!("../languages/list");

/// The compiled-in `languages/` directory: `(locale, file)`.
///
/// The files are the client-scope half of upstream's, fetched from
/// `github.com/plasmoapp/plasmo-voice-crowdin` (branch `pv`, the source of
/// `BuildConstants.GITHUB_CROWDIN_URL`) plus the `en_us.toml` that ships in the jar.
const FILES: &[(&str, &str)] = &[
    ("cs_cz", include_str!("../languages/cs_cz.toml")),
    ("de_de", include_str!("../languages/de_de.toml")),
    ("en_us", include_str!("../languages/en_us.toml")),
    ("es_es", include_str!("../languages/es_es.toml")),
    ("fr_fr", include_str!("../languages/fr_fr.toml")),
    ("he_il", include_str!("../languages/he_il.toml")),
    ("ja_jp", include_str!("../languages/ja_jp.toml")),
    ("ko_kr", include_str!("../languages/ko_kr.toml")),
    ("pl_pl", include_str!("../languages/pl_pl.toml")),
    ("pt_br", include_str!("../languages/pt_br.toml")),
    ("ru_ru", include_str!("../languages/ru_ru.toml")),
    ("sr_sp", include_str!("../languages/sr_sp.toml")),
    ("tr_tr", include_str!("../languages/tr_tr.toml")),
    ("tt_ru", include_str!("../languages/tt_ru.toml")),
    ("uk_ua", include_str!("../languages/uk_ua.toml")),
    ("vi_vn", include_str!("../languages/vi_vn.toml")),
    ("zh_cn", include_str!("../languages/zh_cn.toml")),
    ("zh_tw", include_str!("../languages/zh_tw.toml")),
];

/// The locales this server can answer in, in `languages/list` order.
pub fn locales() -> impl Iterator<Item = &'static str> {
    LIST.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// `getClientLanguage(languageName)` — the flattened client scope of a locale, sorted by
/// key so the packet is deterministic.
///
/// Mirrors `BaseVoiceServer.java:248-253` plus the translator's `getLanguage`:
///
/// * `forced` — `voice.forcedLanguage`. When non-empty, **every** answer is this locale
///   (gaps cannot exist; a locale we do not ship collapses to the fallback), whatever the
///   client asked for.
/// * `default` — `voice.defaultLanguage`. The base table a normal request is answered
///   from; the requested locale's own value wins and the default only fills gaps, exactly
///   like `fillMissing`'s `putIfAbsent`.
/// * An unknown (or empty) locale is answered in the default — the same as `getLanguage`
///   recursing with `null` and landing on the translator's default.
#[must_use]
pub fn client_language(requested: &str, forced: &str, default: &str) -> Vec<(String, String)> {
    let default = normalized(default);
    // Empty means "not forced"; only a non-empty value is normalized.
    let forced = if forced.trim().is_empty() {
        String::new()
    } else {
        normalized(forced)
    };

    if !forced.is_empty() {
        // A forced language is the whole answer. If we do not ship it, there is nothing
        // to answer in — fall through to the default, like the translator would.
        let mut table = scope_of(&forced);
        if table.is_empty() {
            table = scope_of(&default);
        }
        return table.into_iter().collect();
    }

    let mut table = scope_of(&default);

    let requested = normalized(requested);
    if requested != default && !requested.is_empty() {
        for (key, value) in scope_of(&requested) {
            // `fillMissing` uses `putIfAbsent` in the other direction: the locale's own
            // value wins, the default only fills gaps.
            table.insert(key, value);
        }
    }

    table.into_iter().collect()
}

/// Lowercases and trims a locale, with the shipped fallback standing in for garbage.
fn normalized(locale: &str) -> String {
    let locale = locale.trim().to_ascii_lowercase();
    if locale.is_empty() {
        FALLBACK_LANGUAGE.to_string()
    } else {
        locale
    }
}

/// The flattened client scope of one locale, or an empty table when it is not shipped.
fn scope_of(locale: &str) -> BTreeMap<String, String> {
    FILES
        .iter()
        .find(|(code, _)| *code == locale)
        .map(|(_, text)| parse_client_scope(text))
        .unwrap_or_default()
}

/// The flattened `client` scope of one `languages/<locale>.toml`.
///
/// Reproduces `VoiceServerLanguage.languageToMapOfStrings`: nested tables are joined with
/// `.`, the scope name itself is dropped, and other scopes (`server`, which this server
/// does not speak) are ignored. So
///
/// ```toml
/// [client.pv.activation]
/// proximity = "附近"
/// ```
///
/// becomes `pv.activation.proximity = 附近`.
///
/// The reader handles the subset these files are written in — table headers, `key =
/// "basic string"`, blank lines and `#` comments — and unescapes the escapes a TOML basic
/// string allows. A value that is not a basic string is skipped rather than guessed at.
fn parse_client_scope(text: &str) -> BTreeMap<String, String> {
    let mut table: Option<String> = None;
    let mut out = BTreeMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            table = client_table_path(header.trim());
            continue;
        }

        let (Some(table), Some((key, value))) = (table.as_deref(), line.split_once('=')) else {
            continue;
        };
        let Some(value) = basic_string(value.trim()) else {
            continue;
        };

        let key = key.trim();
        out.insert(
            if table.is_empty() {
                key.to_string()
            } else {
                format!("{table}.{key}")
            },
            value,
        );
    }

    out
}

/// `"pv.activation"` for a `[client.pv.activation]` header — `Some("")` for a bare
/// `[client]`, and `None` for every other scope.
fn client_table_path(header: &str) -> Option<String> {
    let rest = header.strip_prefix("client")?;
    match rest.strip_prefix('.') {
        Some(path) => Some(path.to_string()),
        None if rest.is_empty() => Some(String::new()),
        None => None,
    }
}

/// A TOML basic string, unescaped — `None` when the value is not one.
fn basic_string(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }

        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            // `\uXXXX` and `\b`/`\f` do not appear in the files we ship; leaving them
            // verbatim is a visible wrong string rather than a silent misparse.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PROXIMITY_TRANSLATION;

    /// `languages/list` and the `include_str!` table are two halves of one directory.
    #[test]
    fn the_language_list_matches_the_files() {
        let listed: Vec<&str> = locales().collect();
        let shipped: Vec<&str> = FILES.iter().map(|(code, _)| *code).collect();

        assert_eq!(
            listed, shipped,
            "languages/list must name exactly the embedded files, in order"
        );
        assert!(
            listed.contains(&FALLBACK_LANGUAGE),
            "the fallback locale must be one we ship"
        );
    }

    /// Every file parses, and every one of them defines the key the volume tab renders.
    #[test]
    fn every_locale_speaks_the_proximity_line() {
        for (code, text) in FILES {
            let scope = parse_client_scope(text);
            assert_eq!(
                scope.keys().collect::<Vec<_>>(),
                vec![PROXIMITY_TRANSLATION],
                "{code}.toml must flatten to exactly the source-line translation key"
            );
            assert!(
                !scope[PROXIMITY_TRANSLATION].is_empty(),
                "{code}.toml must not translate {PROXIMITY_TRANSLATION} to an empty string"
            );
        }
    }

    /// The lookup lowercases the request, so Minecraft's own `zh_cn` finds our file.
    #[test]
    fn a_locale_is_looked_up_case_insensitively() {
        let zh = client_language("zh_cn", "", FALLBACK_LANGUAGE);
        assert_eq!(
            zh,
            vec![(PROXIMITY_TRANSLATION.to_string(), "附近".to_string())]
        );
        assert_eq!(
            client_language("ZH_CN", "", FALLBACK_LANGUAGE),
            zh,
            "the lookup must be case-insensitive, like getLanguage's lowercase()"
        );
        assert_eq!(
            client_language(" ru_ru ", "", FALLBACK_LANGUAGE),
            vec![(PROXIMITY_TRANSLATION.to_string(), "Локальный".to_string())],
            "surrounding whitespace must not defeat the lookup"
        );
    }

    /// A locale we do not ship is answered in the fallback locale, never with an empty map.
    #[test]
    fn an_unknown_locale_falls_back_to_english() {
        let english = vec![(PROXIMITY_TRANSLATION.to_string(), "Proximity".to_string())];

        for unknown in ["", "  ", "xx_yy", "zh"] {
            assert_eq!(
                client_language(unknown, "", FALLBACK_LANGUAGE),
                english,
                "{unknown:?} must fall back to {FALLBACK_LANGUAGE}"
            );
        }
    }

    /// The configured `defaultLanguage` replaces the built-in fallback entirely: an
    /// unknown request is answered in *that* locale, and its own table fills gaps for a
    /// shipped locale the way `en_us` used to.
    #[test]
    fn a_configured_default_language_takes_over_the_fallback() {
        let zh = vec![(PROXIMITY_TRANSLATION.to_string(), "附近".to_string())];
        assert_eq!(client_language("xx_yy", "", "zh_cn"), zh);

        let ru = vec![(PROXIMITY_TRANSLATION.to_string(), "Локальный".to_string())];
        assert_eq!(
            client_language("ru_ru", "", "zh_cn"),
            ru,
            "the requested locale's own value must win over the default"
        );
        assert_eq!(
            client_language("zh_cn", "", "ru_ru"),
            zh,
            "the requested locale's own value must win over the default"
        );
    }

    /// A configured `forcedLanguage` is the whole answer: the client's request is ignored
    /// (`BaseVoiceServer.java:248-253`), and an unshipped forced locale collapses to the
    /// fallback rather than to an empty map.
    #[test]
    fn a_configured_forced_language_overrides_every_request() {
        let ru = vec![(PROXIMITY_TRANSLATION.to_string(), "Локальный".to_string())];
        for requested in ["zh_cn", "en_us", "xx_yy", ""] {
            assert_eq!(
                client_language(requested, " ru_ru ", FALLBACK_LANGUAGE),
                ru,
                "{requested:?} must be answered in the forced locale"
            );
        }
        assert_eq!(
            client_language("zh_cn", "xx_yy", FALLBACK_LANGUAGE),
            vec![(PROXIMITY_TRANSLATION.to_string(), "Proximity".to_string())],
            "an unshipped forced locale falls back, never answers empty"
        );
    }

    /// The parser drops the scope name, joins nested tables with `.`, and leaves the
    /// `server` scope alone.
    #[test]
    fn the_parser_flattens_only_the_client_scope() {
        let scope = parse_client_scope(
            r#"
            # a comment
            [server.pv.error]
            no_permissions = "nope"

            [client.pv.activation]
            proximity = "Proximity"

              [client.pv.mutes]
              temporary = "muted"

            [client]
            bare = "client root"
            "#,
        );

        assert_eq!(
            scope.keys().collect::<Vec<_>>(),
            vec!["bare", "pv.activation.proximity", "pv.mutes.temporary"]
        );
        assert_eq!(scope["pv.activation.proximity"], "Proximity");
        assert!(
            !scope.keys().any(|key| key.contains("no_permissions")),
            "the server scope must not leak into the client table"
        );
    }

    /// Escapes inside a basic string are unescaped, and a non-string value is skipped.
    #[test]
    fn basic_strings_are_unescaped() {
        let scope = parse_client_scope(
            "[client.pv.test]\nquoted = \"a \\\"b\\\" \\\\ c\"\nnewline = \"a\\nb\"\nnumber = 3\n",
        );

        assert_eq!(scope["pv.test.quoted"], r#"a "b" \ c"#);
        assert_eq!(scope["pv.test.newline"], "a\nb");
        assert!(
            !scope.contains_key("pv.test.number"),
            "a non-string value is skipped, not guessed at"
        );
    }
}
