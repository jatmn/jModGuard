//! Text normalization for bypass-resistant badword matching.
//!
//! Pipeline: unicode NFD -> lowercase -> strip zero-width/format chars ->
//! leetspeak substitution -> optional repeated-character collapse.

use unicode_normalization::UnicodeNormalization;

/// Characters commonly used to visually substitute letters.
/// Applied after lowercase, so only lowercase targets are needed.
fn leetspeak(c: char) -> Option<char> {
    Some(match c {
        '0' => 'o',
        '1' => 'i',
        '3' => 'e',
        '4' => 'a',
        '5' => 's',
        '7' => 't',
        '8' => 'b',
        '9' => 'g',
        '2' => 'z',
        '6' => 'g',
        '@' => 'a',
        '$' => 's',
        '!' => 'i',
        '|' => 'l',
        '€' => 'e',
        '£' => 'e',
        '¢' => 'c',
        '+' => 't',
        _ => return None,
    })
}

pub(crate) fn is_zero_width(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'
            | '\u{200C}'
            | '\u{200D}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'
            | '\u{202B}'
            | '\u{202C}'
            | '\u{202D}'
            | '\u{202E}'
            | '\u{2060}'
            | '\u{FEFF}'
            | '\u{00AD}'
            | '\u{034F}'
            | '\u{180E}'
    )
}

/// Nonspacing combining marks (Mn category ranges): after NFD decomposition
/// these are the accent codepoints, e.g. é -> e + U+0301.
fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}'
        | '\u{1AB0}'..='\u{1AFF}'
        | '\u{1DC0}'..='\u{1DFF}'
        | '\u{20D0}'..='\u{20FF}')
}

/// Normalizes text for matching:
/// - lowercased, NFD normalized (so composed accented chars decompose)
/// - zero-width characters and combining marks removed
/// - leetspeak digits/symbols substituted
/// - repeated characters collapsed when `collapse_repeats` is true
pub fn normalize_text(input: &str, collapse_repeats: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev: Option<char> = None;
    for c in input.nfd().flat_map(|c| c.to_lowercase()) {
        if is_zero_width(c) || is_combining_mark(c) {
            continue;
        }
        let c = leetspeak(c).unwrap_or(c);
        if collapse_repeats && prev == Some(c) {
            continue;
        }
        prev = Some(c);
        out.push(c);
    }
    out
}

/// Normalized without letter-by-letter dedup (safer default: only collapse
/// when the admin explicitly enables it per pattern is not supported, so we
/// collapse nothing by default — see `matches_badword`).
pub fn normalize_lenient(input: &str) -> String {
    normalize_text(input, false)
}

/// Fully compacted form: leetspeak + all whitespace removed, so spaced-out
/// bypass attempts like "s c a m" match `scam`.
pub fn normalize_compact(input: &str) -> String {
    normalize_text(input, false)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Checks a message against a compiled pattern, testing the raw text, the
/// normalized form, and the compacted (space-stripped) form.
#[allow(dead_code)]
pub fn matches_badword(raw: &str, normalized: &str, pattern: &regex::Regex) -> bool {
    pattern.is_match(raw)
        || pattern.is_match(normalized)
        || pattern.is_match(&normalize_compact(raw))
}

// ---------------------------------------------------------------------------
// Untrusted / WebUI regex input (ReDoS / resource guards)
// ---------------------------------------------------------------------------

/// Max length of a user-supplied regex pattern (WebUI tester + badword add).
pub const USER_REGEX_MAX_PATTERN_BYTES: usize = 512;
/// Max length of sample text for the live regex tester.
pub const USER_REGEX_MAX_SAMPLE_BYTES: usize = 4096;
/// Approximate compiled NFA size cap (bytes) for user patterns.
const USER_REGEX_SIZE_LIMIT: usize = 100_000;
/// AST nesting depth cap for user patterns.
const USER_REGEX_NEST_LIMIT: u32 = 50;

/// Compiles a user-supplied regex with length + compile-size limits.
///
/// The `regex` crate matches in linear time, but untrusted patterns can still
/// exhaust memory/CPU at compile time (counted repetitions, deep nesting).
/// Call this for WebUI tester and before persisting badword patterns.
pub fn compile_user_regex(pattern: &str) -> Result<regex::Regex, String> {
    if pattern.is_empty() {
        return Err("pattern is empty".into());
    }
    if pattern.len() > USER_REGEX_MAX_PATTERN_BYTES {
        return Err(format!(
            "pattern too long (max {USER_REGEX_MAX_PATTERN_BYTES} bytes)"
        ));
    }
    regex::RegexBuilder::new(pattern)
        .size_limit(USER_REGEX_SIZE_LIMIT)
        .dfa_size_limit(USER_REGEX_SIZE_LIMIT)
        .nest_limit(USER_REGEX_NEST_LIMIT)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

/// Compile ordinary entries into the existing regex store. Named capture preserves
/// the display text without adding a second list or rewriting legacy patterns.
pub fn literal_badword_pattern(word: &str) -> Result<String, String> {
    let word = word.trim().to_lowercase();
    if word.is_empty() || word.len() > 200 {
        return Err("each word or phrase must be 1–200 bytes".into());
    }
    let normalized = normalize_compact(&word);
    if normalized.is_empty() {
        return Err("word or phrase is empty after normalization".into());
    }
    static WORD_CHARACTER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\w").expect("constant word-character regex")
    });
    let boundary = |c: char| {
        if WORD_CHARACTER.is_match(&c.to_string()) {
            r"\b"
        } else {
            ""
        }
    };
    let bounded = |text: &str, body: String| {
        format!(
            "{}{}{}",
            boundary(text.chars().next().unwrap()),
            body,
            boundary(text.chars().last().unwrap())
        )
    };
    let raw = bounded(&word, format!("(?P<jmg_word>{})", regex::escape(&word)));
    // Optional spaces preserve word boundaries even in a spaced-out message
    // surrounded by other words, where compacting the whole message loses them.
    let spaced = normalized
        .chars()
        .map(|c| regex::escape(&c.to_string()))
        .collect::<Vec<_>>()
        .join(r"\s*");
    let pattern = format!("(?i:{raw}|{})", bounded(&normalized, spaced));
    compile_user_regex(&pattern)?;
    Ok(pattern)
}

/// Recognize only our canonical encoding. Other stored regexes retain their
/// exact text and semantics, including any unrelated named captures.
pub fn badword_literal(pattern: &str) -> Option<String> {
    let (_, encoded) = pattern.split_once("(?P<jmg_word>")?;
    let mut chars = encoded.chars();
    let mut word = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' => word.push(chars.next()?),
            ')' => break,
            _ => word.push(c),
        }
    }
    (literal_badword_pattern(&word).ok()?.as_str() == pattern).then_some(word)
}

/// Bound and validate a whole paste before any mutation. Empty separators are
/// ignored and duplicate canonical literals in the same paste are collapsed.
pub fn badword_patterns(input: &str) -> Result<Vec<String>, String> {
    if input.len() > USER_REGEX_MAX_SAMPLE_BYTES {
        return Err("word list too long (max 4096 bytes)".into());
    }
    let words: Vec<_> = input
        .split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() || words.len() > 32 {
        return Err("enter 1–32 words or phrases separated by commas or newlines".into());
    }
    let mut patterns = Vec::new();
    for word in words {
        let pattern = literal_badword_pattern(word)?;
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leetspeak_and_casing() {
        let n = normalize_lenient("H3ll0 W0r1d @$$h0l3");
        // '1' maps to 'i' (ambiguous with 'l'; admins can use regex classes
        // like wor[il]d when both readings matter).
        assert_eq!(n, "hello worid asshole");
    }

    #[test]
    fn zero_width_stripped() {
        let n = normalize_lenient("s\u{200B}c\u{200D}am");
        assert_eq!(n, "scam");
    }

    #[test]
    fn repeated_chars_collapse() {
        let n = normalize_text("hheellllooo", true);
        assert_eq!(n, "helo");
    }

    #[test]
    fn unicode_decompose() {
        let n = normalize_lenient("café résumé");
        assert_eq!(n, "cafe resume");
    }

    #[test]
    fn matches_obfuscated_word() {
        let re = regex::Regex::new(r"scam").unwrap();
        assert!(matches_badword(
            "s c a m",
            &normalize_lenient("s c a m"),
            &re
        ));
        assert!(matches_badword("s5c@m", &normalize_lenient("s5c@m"), &re));
    }

    #[test]
    fn compile_user_regex_accepts_simple_patterns() {
        let re = compile_user_regex(r"(?i)scam|pump").unwrap();
        assert!(re.is_match("Pump and dump"));
    }

    #[test]
    fn compile_user_regex_rejects_empty_and_oversized() {
        assert!(compile_user_regex("").is_err());
        let huge = "a".repeat(USER_REGEX_MAX_PATTERN_BYTES + 1);
        let err = compile_user_regex(&huge).unwrap_err();
        assert!(err.contains("too long"), "unexpected: {err}");
    }

    #[test]
    fn compile_user_regex_rejects_pathological_compile() {
        // Counted repetition expands at compile time past size_limit.
        let err = compile_user_regex(r"\w{100000}").unwrap_err();
        assert!(
            err.contains("invalid regex") || err.contains("size"),
            "unexpected: {err}"
        );
    }
}

#[cfg(test)]
mod badword_setup_tests {
    use super::*;
    #[test]
    fn literal_badwords_escape_bound_and_normalize() {
        for (word, yes, no) in [
            ("scam", "A SCAM!", "scammer"),
            ("pump and dump", "pump  and dump", "pump and dumpling"),
            ("c++", "use c++ today", "ccc"),
            ("a.b[0]", "a.b[0]", "axb0"),
            (r"a\b(c)|d.*", r"a\b(c)|d.*", "abcd"),
            ("café", "CAFE", "cafeteria"),
            ("cafe\u{301}", "cafe\u{301}", "cafe\u{301}teria"),
        ] {
            let pattern = literal_badword_pattern(word).unwrap();
            let re = compile_user_regex(&pattern).unwrap();
            assert!(matches_badword(yes, &normalize_lenient(yes), &re), "{word}");
            assert!(!matches_badword(no, &normalize_lenient(no), &re), "{word}");
            assert_eq!(badword_literal(&pattern).as_deref(), Some(word));
        }
        let re = compile_user_regex(&literal_badword_pattern("scam").unwrap()).unwrap();
        for sample in ["5c@m", "s c a m", "a s c a m here", "sc\u{200b}am", "scám"] {
            assert!(
                matches_badword(sample, &normalize_lenient(sample), &re),
                "{sample}"
            );
        }
    }
    #[test]
    fn literal_badword_lists_validate_and_deduplicate() {
        let patterns = badword_patterns(" Scam, pump and dump\nscam\r\n , c++ ").unwrap();
        assert_eq!(patterns.len(), 3);
        assert_eq!(badword_literal(&patterns[0]).as_deref(), Some("scam"));
        for input in ["", "  \n,\t", "\u{200b}"] {
            assert!(badword_patterns(input).is_err());
        }
        assert!(badword_patterns(&"x".repeat(60)).is_ok());
        assert!(
            badword_patterns(&"x".repeat(200)).is_err(),
            "compiled pattern length is also bounded"
        );
        assert!(badword_patterns(&"x".repeat(201)).is_err());
        assert!(
            badword_patterns(
                &(0..33)
                    .map(|n| format!("word{n}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
            .is_err()
        );
        assert!(badword_patterns(&"x".repeat(4097)).is_err());
        for legacy in ["scam", "(?i)scam", r"^sc[a@]m$", "["] {
            assert_eq!(badword_literal(legacy), None);
        }
    }
}
