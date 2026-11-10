//! Exact saved samples using Badwords normalization, plus DNS/path wildcards.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;

pub const MAX_ENTRIES: usize = 1024;
pub const MAX_SAMPLE_BYTES: usize = 4096;
pub const MAX_URL_BYTES: usize = 512;
pub const MAX_NOTE_BYTES: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub id: i64,
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub guild_id: i64,
    pub kind: String,
    pub value: String,
    pub note: Option<String>,
}

impl Entry {
    pub fn new(kind: &str, value: &str, note: Option<&str>) -> Result<Self> {
        let value = value.trim();
        let value = match kind {
            "sample" => {
                ensure!(
                    !crate::normalize::normalize_compact(value).is_empty(),
                    "sample is empty"
                );
                ensure!(
                    value.len() <= MAX_SAMPLE_BYTES,
                    "sample exceeds {MAX_SAMPLE_BYTES} bytes"
                );
                value.to_owned()
            }
            "url" => UrlPattern::parse(value)?.canonical(),
            _ => anyhow::bail!("kind must be sample or url"),
        };
        let note = note.map(str::trim).filter(|s| !s.is_empty());
        ensure!(
            note.is_none_or(|s| s.len() <= MAX_NOTE_BYTES && !s.chars().any(char::is_control)),
            "note must be at most {MAX_NOTE_BYTES} bytes without control characters"
        );
        Ok(Self {
            id: 0,
            guild_id: 0,
            kind: kind.into(),
            value,
            note: note.map(str::to_owned),
        })
    }

    pub fn match_key(&self) -> String {
        if self.kind == "sample" {
            crate::normalize::normalize_compact(&self.value)
        } else {
            self.value.clone()
        }
    }
}

#[derive(Debug)]
struct UrlPattern {
    host: String,
    subdomains: bool,
    path: String,
}
impl UrlPattern {
    fn parse(raw: &str) -> Result<Self> {
        ensure!(
            raw.len() <= MAX_URL_BYTES && !raw.is_empty(),
            "URL pattern must be 1–{MAX_URL_BYTES} bytes"
        );
        ensure!(
            raw.is_ascii()
                && !raw.chars().any(|c| c.is_control() || c.is_whitespace())
                && !raw.contains(['?', '#', '@', '\\']),
            "invalid URL pattern"
        );
        let without_scheme = if raw
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
        {
            &raw[8..]
        } else if raw
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
        {
            &raw[7..]
        } else {
            raw
        };
        let (host, path) = without_scheme
            .split_once('/')
            .map_or((without_scheme, "/*".to_owned()), |(h, p)| {
                (h, format!("/{p}"))
            });
        let (subdomains, host) = host.strip_prefix("*.").map_or((false, host), |h| (true, h));
        let labels: Vec<_> = host.split('.').collect();
        ensure!(
            host.len() <= 253
                && labels.len() >= 2
                && labels.iter().all(|label| !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
                && labels
                    .last()
                    .is_some_and(|s| s.len() >= 2 && s.chars().all(|c| c.is_ascii_alphabetic())),
            "expected a DNS host; only a leading *. host wildcard is supported"
        );
        ensure!(
            !path.split('/').any(|s| s == "." || s == "..")
                && !path.contains(['<', '>', '"', '\'', '`', '{', '}', '[', ']']),
            "invalid URL path pattern"
        );
        Ok(Self {
            host: host.to_ascii_lowercase(),
            subdomains,
            path,
        })
    }
    fn canonical(&self) -> String {
        format!(
            "{}{}{}",
            if self.subdomains { "*." } else { "" },
            self.host,
            self.path
        )
    }
    fn matches(&self, url: &url::Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host_match = if self.subdomains {
            host.strip_suffix(&self.host)
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.'))
        } else {
            host.eq_ignore_ascii_case(&self.host)
        };
        host_match && path_glob(&self.path, url.path())
    }
}

// Literal path segments with * as the only metacharacter. Never user regexes.
fn path_glob(pattern: &str, path: &str) -> bool {
    let (p, s) = (pattern.as_bytes(), path.as_bytes());
    let (mut pi, mut si, mut star, mut retry) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            retry = si;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(at) = star {
            retry += 1;
            si = retry;
            pi = at + 1;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

static URL_TOKENS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"[^\s<>()\[\]{}\"'`]+"#).expect("static URL token expression")
});

// Emphasis markers belong to Discord message formatting, not the URL path.
// Remove only paired outer markers so a literal trailing path * stays literal.
fn url_token_text(mut token: &str) -> &str {
    token = token.trim_end_matches(['.', ',', ';', '!', '?', ':']);
    while let Some(inner) = ["**", "__", "~~", "*", "_", "~"].iter().find_map(|marker| {
        token
            .strip_prefix(marker)
            .and_then(|inner| inner.strip_suffix(marker))
    }) {
        token = inner;
    }
    token.trim_end_matches(['.', ',', ';', '!', '?', ':'])
}

#[derive(Default)]
pub struct CompiledList {
    raw: HashSet<String>,
    normalized: HashSet<String>,
    compact: HashSet<String>,
    urls: Vec<UrlPattern>,
}
impl CompiledList {
    pub fn new(rows: &[Entry]) -> Self {
        let mut out = Self::default();
        for row in rows.iter().take(MAX_ENTRIES * 2) {
            // Restored/legacy invalid values are not enforceable.
            let Ok(entry) = Entry::new(&row.kind, &row.value, row.note.as_deref()) else {
                continue;
            };
            if entry.kind == "sample" {
                out.raw.insert(entry.value.clone());
                out.normalized
                    .insert(crate::normalize::normalize_lenient(&entry.value));
                out.compact
                    .insert(crate::normalize::normalize_compact(&entry.value));
            } else if let Ok(pattern) = UrlPattern::parse(&entry.value) {
                out.urls.push(pattern);
            }
        }
        out
    }
    pub fn matches(&self, content: &str) -> bool {
        self.matches_forms(
            content,
            &crate::normalize::normalize_lenient(content),
            &crate::normalize::normalize_compact(content),
        )
    }
    pub fn matches_forms(&self, raw: &str, normalized: &str, compact: &str) -> bool {
        if self.raw.contains(raw.trim())
            || self.normalized.contains(normalized.trim())
            || self.compact.contains(compact)
        {
            return true;
        }
        if self.urls.is_empty() {
            return false;
        }
        URL_TOKENS.find_iter(raw).any(|token| {
            let token = url_token_text(token.as_str());
            let parsed = if token
                .get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
                || token
                    .get(..8)
                    .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
            {
                url::Url::parse(token)
            } else {
                url::Url::parse(&format!("https://{token}"))
            };
            parsed.is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.username().is_empty()
                    && url.password().is_none()
                    && self.urls.iter().any(|p| p.matches(&url))
            })
        })
    }
}

pub fn action() -> crate::types::ActionDef {
    crate::types::ActionDef {
        at: 1,
        action: "ban".into(),
        duration_minutes: 0,
        delete_days: 0,
        purge_last_hour: true,
        reason: "Known spam message".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const STOCKS: &str = "📈 **Looking for a community focused on stocks and trading?**\n\nJoin our growing community to learn from experienced investors, share market insights, discuss stock ideas, and exchange trading strategies.\nNo spam or aggressive promotion—just practical market discussions.\nWe welcome active investors and value your thoughts and feedback.\n👇 Interested? Join us here:";
    const AI: &str = "I used to work as Full Stack AI developer at several software companies but I’m currently available and working remotely because my last contract ended. Right now I am resting at home. If you have any AI projects or need support, I’d be happy to help, just reach out.";
    fn list(kind: &str, value: &str) -> CompiledList {
        CompiledList::new(&[Entry::new(kind, value, None).unwrap()])
    }
    #[test]
    fn known_spam_samples_match_normalized_whole_messages_only() {
        for sample in [STOCKS, AI] {
            let compiled = list("sample", sample);
            for variant in [
                sample.to_owned(),
                sample.to_uppercase(),
                sample.replace(' ', " \n "),
                sample.replace('e', "3").replace('a', "@"),
                sample.replace('o', "ó").replace('i', "i\u{200b}"),
                sample.chars().filter(|c| !c.is_whitespace()).collect(),
            ] {
                assert!(compiled.matches(&variant), "{variant}");
            }
            assert!(!compiled.matches("I enjoy developing software and trading ideas."));
            assert!(!compiled.matches(&format!("Someone wrote: {sample}")));
            assert!(!compiled.matches(""));
        }
    }
    #[test]
    fn known_spam_urls_have_host_boundaries_and_case_sensitive_paths() {
        let compiled = list("url", "https://TinyURL.com/*");
        for text in [
            "https://tinyurl.com/abc",
            "tinyurl.com/xyz",
            "go <http://TINYURL.COM/a?x=2#here>",
            "[join](https://tinyurl.com/a)",
            "**https://tinyurl.com/a**",
            "__tinyurl.com/a__",
            "__**https://tinyurl.com/a**__.",
            "tinyurl.com",
            "https://tinyurl.com/",
        ] {
            assert!(compiled.matches(text), "{text}");
        }
        for text in [
            "https://nottinyurl.com/a",
            "https://tinyurl.com.evil.test/a",
            "https://sub.tinyurl.com/a",
            "https://tinyurl.com@evil.test/a",
            "https://evil.test/tinyurl.com/a",
            "wordtinyurl.com/a",
            "user@tinyurl.com",
        ] {
            assert!(!compiled.matches(text), "{text}");
        }
        let paths = list("url", "example.com/Promo/*");
        assert!(paths.matches("example.com/Promo/abc"));
        assert!(!paths.matches("example.com/promo/abc"));
        assert!(!paths.matches("example.com/other/abc"));
        let exact = list("url", "example.com/Promo");
        assert!(!exact.matches("https://example.com/Promo*"));
        assert!(exact.matches("**https://example.com/Promo**"));
        let subs = list("url", "*.example.com/*");
        assert!(subs.matches("https://one.two.example.com/a"));
        assert!(!subs.matches("https://example.com/a"));
    }
    #[test]
    fn known_spam_validation_bounds_and_notes() {
        for value in ["", " \n\t", "\u{200b}"] {
            assert!(Entry::new("sample", value, None).is_err());
        }
        assert!(Entry::new("sample", &"a".repeat(4097), None).is_err());
        assert!(Entry::new("sample", &"a".repeat(4096), None).is_ok());
        assert!(Entry::new("sample", AI, Some(&"n".repeat(513))).is_err());
        assert_eq!(
            Entry::new("sample", AI, Some(" note "))
                .unwrap()
                .note
                .as_deref(),
            Some("note")
        );
        assert_eq!(Entry::new("sample", AI, Some(" ")).unwrap().note, None);
        for pattern in [
            "*",
            "com/*",
            "*.com/*",
            "ftp://example.com/*",
            "example.*/*",
            "example.com:443/*",
            "user@example.com/*",
            "example.com/?x=*",
            "example.com/#*",
            "https://",
            "example.com/with space",
            "example.com/../*",
            "example.com/\\*",
        ] {
            assert!(Entry::new("url", pattern, None).is_err(), "{pattern}");
        }
        assert!(Entry::new("other", AI, None).is_err());
        assert_eq!(
            Entry::new("url", "HTTP://TinyURL.com", None).unwrap().value,
            "tinyurl.com/*"
        );
    }
}
