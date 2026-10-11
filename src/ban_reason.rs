//! Opaque dispatch identity carried by Discord's audit reason, never a case ID.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BanAttemptTag(String);

impl Default for BanAttemptTag {
    fn default() -> Self {
        Self::new()
    }
}

impl BanAttemptTag {
    pub fn new() -> Self {
        Self(format!(
            "{:020x}",
            rand::random::<u128>() & ((1_u128 << 80) - 1)
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BanAttemptTag {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        anyhow::ensure!(valid_tag(&value), "invalid ban attempt tag");
        Ok(Self(value))
    }
}

impl From<BanAttemptTag> for String {
    fn from(tag: BanAttemptTag) -> Self {
        tag.0
    }
}

fn valid_tag(value: &str) -> bool {
    value.len() == 20
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn split_tag(reason: &str) -> Option<(&str, &str)> {
    let (tag, rest) = reason.strip_prefix("[jmg:")?.split_once(']')?;
    valid_tag(tag).then_some((tag, rest.strip_prefix(' ').unwrap_or(rest)))
}

pub fn attempt_tag(reason: &str) -> Option<&str> {
    split_tag(reason).map(|(tag, _)| tag)
}

/// Remove copied-back prefixes, including repeated tags, before display or storage.
pub fn without_tag(mut reason: &str) -> &str {
    while let Some((_, rest)) = split_tag(reason) {
        reason = rest;
    }
    reason
}

pub fn discord_reason(reason: &str, tag: Option<&BanAttemptTag>) -> String {
    let reason = without_tag(reason);
    let tagged = match tag {
        Some(tag) => format!("[jmg:{}] {reason}", tag.as_str()),
        None => reason.to_owned(),
    };
    tagged.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_reason_keeps_identity_and_copied_prefixes_do_not_stack() {
        let tag = BanAttemptTag::new();
        let other = BanAttemptTag::new();
        let text = "💬".repeat(512);
        let sent = discord_reason(&text, Some(&tag));
        assert_eq!(sent.chars().count(), 512);
        assert_eq!(attempt_tag(&sent), Some(tag.as_str()));
        assert_eq!(without_tag(&sent), "💬".repeat(485));
        let copied = format!("[jmg:{}] {sent}", other.as_str());
        assert_eq!(discord_reason(&copied, Some(&tag)), sent);
        assert_eq!(
            without_tag("[jmg:not-an-attempt] reason"),
            "[jmg:not-an-attempt] reason"
        );
    }
}
