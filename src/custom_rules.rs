//! Per-guild custom automod rules (`custom:` ids): validation and helpers.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;

use crate::normalize;
use crate::types::{ActionDef, GLOBAL_SCOPE, RuleDef};

pub const CUSTOM_ID_PREFIX: &str = "custom:";
pub const MAX_CUSTOM_RULES_PER_GUILD: usize = 32;
pub const NO_LINKS_OR_MEDIA_PRESET: &str = "no_links_or_media";
/// Server channel kinds whose own ID carries chat messages.
pub const MEDIA_PRESET_CHANNEL_KINDS: &[&str] = &["text", "announcement", "voice", "stage"];
pub const DEFAULT_REPLY_DELETE_SECONDS: u64 = 10;
pub const MAX_REPLY_DELETE_SECONDS: u64 = 300;

const MAX_CUSTOM_ACTIONS: usize = 7;
const MAX_ALLOWED_DOMAINS: usize = 32;
const MAX_REPLY_LENGTH: usize = 2000;

const MAX_NAME_LEN: usize = 80;
const MAX_DESCRIPTION_LEN: usize = 400;
const MAX_REASON_LEN: usize = 200;
const MAX_WORDS: usize = 32;
const MAX_WORD_BYTES: usize = 64;

/// `custom:` plus 8–32 lowercase hex digits.
pub fn is_custom_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix(CUSTOM_ID_PREFIX) else {
        return false;
    };
    let len = rest.len();
    (8..=32).contains(&len)
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b) || (b'a'..=b'f').contains(&b))
        && rest.bytes().all(|b| !b.is_ascii_uppercase())
}

pub fn known_rule_id(id: &str) -> bool {
    crate::types::RULE_IDS.contains(&id) || is_custom_id(id)
}

/// API admission: shape only (no per-guild row count).
pub fn validate_custom_rule_shape(guild_id: i64, id: &str, rule: &RuleDef) -> Result<()> {
    ensure!(
        guild_id != GLOBAL_SCOPE,
        "custom rules cannot use global scope"
    );
    ensure!(is_custom_id(id), "invalid custom rule id `{id}`");
    validate_custom_body(id, rule, true)
}

/// Bot write: same shape checks plus dry-run-on-create when `creating` is true.
pub fn validate_custom_rule_for_write(
    guild_id: i64,
    id: &str,
    rule: &RuleDef,
    creating: bool,
) -> Result<()> {
    ensure!(
        guild_id != GLOBAL_SCOPE,
        "custom rules cannot use global scope"
    );
    ensure!(is_custom_id(id), "invalid custom rule id `{id}`");
    if creating && !rule.dry_run {
        bail!("new custom rules start in dry-run");
    }
    validate_custom_body(id, rule, true)
}

/// A custom rule introduced at a guild (upsert create or a copy onto a server).
/// The stored row starts in dry-run; shape, scope, and the dry-run rule are the
/// same checks as [`validate_custom_rule_for_write`].
pub fn custom_rule_for_create(guild_id: i64, id: &str, mut rule: RuleDef) -> Result<RuleDef> {
    rule.dry_run = true;
    validate_custom_rule_for_write(guild_id, id, &rule, true)?;
    Ok(rule)
}

pub fn ensure_room_for_new_custom_rules(existing: usize, adding: usize) -> Result<()> {
    if adding == 0 {
        return Ok(());
    }
    let total = existing
        .checked_add(adding)
        .context("custom rule count overflow")?;
    if total > MAX_CUSTOM_RULES_PER_GUILD {
        bail!("at most {MAX_CUSTOM_RULES_PER_GUILD} custom rules per guild");
    }
    Ok(())
}

fn validate_custom_body(_id: &str, rule: &RuleDef, compile_pattern: bool) -> Result<()> {
    let name = rule.name.trim();
    ensure!(!name.is_empty(), "rule name is required");
    ensure!(
        name.len() <= MAX_NAME_LEN,
        "rule name must be at most {MAX_NAME_LEN} characters"
    );
    ensure!(
        rule.description.len() <= MAX_DESCRIPTION_LEN,
        "rule description must be at most {MAX_DESCRIPTION_LEN} characters"
    );

    ensure!(
        (1..=MAX_CUSTOM_ACTIONS).contains(&rule.actions.len()),
        "custom rules must have 1–{MAX_CUSTOM_ACTIONS} actions"
    );
    let kind = rule
        .params
        .get("kind")
        .and_then(|v| v.as_str())
        .context("custom rule params.kind is required")?;
    if requires_include_channels(rule) {
        ensure!(
            kind == "channel_content",
            "the no links or media preset requires channel content"
        );
        ensure!(
            !rule.enabled || rule.include_channels.iter().any(|channel| *channel != 0),
            "pick at least one channel before enabling the no links or media preset"
        );
    }
    let mut seen = std::collections::HashSet::new();
    for action in &rule.actions {
        ensure!(
            seen.insert(action.action.as_str()),
            "custom actions cannot be repeated"
        );
        validate_action(action)?;
        if action.action == "channel_reply" {
            ensure!(
                kind != "honeypot_role",
                "channel replies require a message trigger"
            );
            channel_reply(rule)?;
        }
    }
    match kind {
        "pattern" => {
            let pattern = rule
                .params
                .get("pattern")
                .and_then(|v| v.as_str())
                .context("pattern rules require params.pattern")?;
            if compile_pattern {
                normalize::compile_user_regex(pattern).map_err(|e| anyhow!(e))?;
            }
        }
        "word" => validate_words(rule)?,
        "honeypot_role" => {
            let _role = parse_role_id(rule)?;
        }
        "channel_content" => validate_channel_content(rule)?,
        "everyone_attempt" => {}
        other => bail!("unknown custom rule kind `{other}`"),
    }

    Ok(())
}

fn validate_action(action: &ActionDef) -> Result<()> {
    ensure!(
        action.reason.len() <= MAX_REASON_LEN,
        "action reason must be at most {MAX_REASON_LEN} characters"
    );
    ensure!(
        action.action == "ban" || !action.purge_last_hour,
        "purge_last_hour is only valid for ban"
    );
    match action.action.as_str() {
        "log" | "delete" | "warn" | "kick" | "channel_reply" => {
            ensure!(
                action.delete_days == 0,
                "delete_days must be 0 for this action"
            );
        }
        "timeout" => {
            ensure!(
                (1..=40320).contains(&action.duration_minutes),
                "timeout duration_minutes must be 1–40320"
            );
            ensure!(action.delete_days == 0, "delete_days must be 0 for timeout");
        }
        "ban" => {
            ensure!(action.delete_days <= 7, "ban delete_days must be 0–7");
        }
        other => bail!("unsupported custom action `{other}`"),
    }
    Ok(())
}

fn validate_words(rule: &RuleDef) -> Result<()> {
    let words = rule
        .params
        .get("words")
        .and_then(|v| v.as_array())
        .context("word rules require params.words array")?;
    ensure!(
        !words.is_empty() && words.len() <= MAX_WORDS,
        "word rules need 1–{MAX_WORDS} words"
    );
    for w in words {
        let s = w.as_str().context("words must be strings")?;
        ensure!(!s.is_empty(), "words cannot be empty");
        ensure!(
            s.len() <= MAX_WORD_BYTES,
            "each word must be at most {MAX_WORD_BYTES} bytes"
        );
    }
    Ok(())
}

pub fn content_toggle(rule: &RuleDef, key: &str) -> bool {
    rule.params.get(key).and_then(Value::as_bool) == Some(true)
}

pub fn requires_include_channels(rule: &RuleDef) -> bool {
    rule.params.get("preset").and_then(Value::as_str) == Some(NO_LINKS_OR_MEDIA_PRESET)
}

pub(crate) fn validate_media_preset_channels(
    ids: &[u64],
    channels: &[crate::db::guild_data::ServerChannelName],
) -> Result<()> {
    for id in ids {
        let id = id.to_string();
        ensure!(
            channels.iter().any(|channel| {
                channel.id == id && MEDIA_PRESET_CHANNEL_KINDS.contains(&channel.kind.as_str())
            }),
            "Channel {id} is unavailable for the no links or media preset. Pick a supported channel from the server list."
        );
    }
    Ok(())
}

fn validate_channel_content(rule: &RuleDef) -> Result<()> {
    ensure!(
        ["links", "attachments", "gifs", "stickers"]
            .iter()
            .any(|key| content_toggle(rule, key)),
        "channel_content needs links, attachments, GIFs, or stickers enabled"
    );
    if let Some(domains) = rule.params.get("allowed_domains") {
        let domains = domains
            .as_array()
            .context("allowed_domains must be an array")?;
        ensure!(
            domains.len() <= MAX_ALLOWED_DOMAINS,
            "at most {MAX_ALLOWED_DOMAINS} domains are allowed"
        );
        for domain in domains {
            let domain = domain.as_str().context("allowed domains must be strings")?;
            ensure!(
                valid_allowed_domain(domain),
                "use a bare domain in allowed_domains"
            );
        }
    }
    Ok(())
}

fn valid_allowed_domain(domain: &str) -> bool {
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    domain.len() <= 253
        && domain.contains('.')
        && domain.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        && matches!(url::Host::parse(domain), Ok(url::Host::Domain(_)))
}

/// Tests URL tokens already recognized by the message detector. Entries allow
/// exact hosts, including GIF hosts, without exempting other URLs in a message.
pub fn channel_link_blocked(rule: &RuleDef, token: &str) -> bool {
    let links = content_toggle(rule, "links");
    let gifs = content_toggle(rule, "gifs");
    let allowed_domains = rule.params["allowed_domains"]
        .as_array()
        .filter(|domains| !domains.is_empty());
    if !links && !gifs {
        return false;
    }
    if links && allowed_domains.is_none() {
        return true;
    }
    // The detector also recognizes www. links and Discord invites without a scheme.
    let token = token.trim_end_matches([')', ']', '}', ',', '!', '?']);
    let parsed = if token.split_once("://").is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    }) {
        url::Url::parse(token)
    } else {
        url::Url::parse(&format!("https://{token}"))
    };
    let Ok(url) = parsed else { return links };
    let Some(domain) = url.domain() else {
        return links;
    };
    let domain = domain.trim_end_matches('.');
    let gif_host = ["tenor.com", "giphy.com", "gph.is"].iter().any(|host| {
        domain == *host
            || domain
                .strip_suffix(host)
                .is_some_and(|prefix| prefix.ends_with('.'))
    });
    let allowed = url.username().is_empty()
        && url.password().is_none()
        && allowed_domains.is_some_and(|domains| {
            domains
                .iter()
                .filter_map(Value::as_str)
                .any(|allowed| allowed.trim_end_matches('.').eq_ignore_ascii_case(domain))
        });
    (links || gifs && gif_host) && !allowed
}

pub fn channel_reply(rule: &RuleDef) -> Result<(&str, u64)> {
    let config = &rule.params["channel_reply"];
    let template = config["template"]
        .as_str()
        .context("channel replies require a message template")?;
    ensure!(
        !template.trim().is_empty(),
        "channel reply text cannot be empty"
    );
    ensure!(
        template.encode_utf16().count() <= MAX_REPLY_LENGTH,
        "channel reply text must be at most {MAX_REPLY_LENGTH} characters"
    );
    let seconds = match config.get("delete_after_seconds") {
        Some(value) => value
            .as_u64()
            .context("reply auto-delete must be a whole number of seconds")?,
        None => DEFAULT_REPLY_DELETE_SECONDS,
    };
    ensure!(
        seconds <= MAX_REPLY_DELETE_SECONDS,
        "reply auto-delete must be 0–{MAX_REPLY_DELETE_SECONDS} seconds"
    );
    Ok((template, seconds))
}

pub fn parse_role_id(rule: &RuleDef) -> Result<u64> {
    let v = rule
        .params
        .get("role_id")
        .context("honeypot_role requires params.role_id")?;
    parse_snowflake_value(v)
}

fn parse_snowflake_value(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| anyhow!("role_id must be a positive integer")),
        Value::String(s) => s
            .trim()
            .parse()
            .map_err(|e| anyhow!("invalid role_id: {e}")),
        _ => bail!("role_id must be a snowflake string or integer"),
    }
}

pub fn custom_words(rule: &RuleDef) -> Vec<String> {
    rule.params
        .get("words")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|w| w.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Whole-word match on literal or normalized text (case-insensitive).
pub fn word_rule_matches(content: &str, words: &[String]) -> bool {
    if words.is_empty() {
        return false;
    }
    let literal = content.to_lowercase();
    let literal_tokens: std::collections::HashSet<&str> = literal
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let normalized = normalize::normalize_lenient(content);
    let tokens: std::collections::HashSet<&str> = normalized
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    words.iter().any(|w| {
        let token = normalize::normalize_lenient(w);
        (!token.is_empty() && tokens.contains(token.as_str()))
            || literal_tokens.contains(w.to_lowercase().as_str())
    })
}

pub fn everyone_attempt_matches(content: &str, mention_everyone_flag: bool) -> bool {
    if mention_everyone_flag {
        return true;
    }
    content
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '@')
        .any(|token| token.eq_ignore_ascii_case("@everyone") || token.eq_ignore_ascii_case("@here"))
}

pub fn rule_scope_skips(
    rule: &RuleDef,
    author_id: u64,
    channel_id: u64,
    member_roles: &[u64],
    is_moderator: bool,
    check_include_channels: bool,
) -> bool {
    if rule.skip_moderators && is_moderator {
        return true;
    }
    if rule.whitelist_users.contains(&author_id)
        || rule.whitelist_channels.contains(&channel_id)
        || rule
            .whitelist_roles
            .iter()
            .any(|r| member_roles.contains(r))
    {
        return true;
    }
    if check_include_channels
        && (requires_include_channels(rule) || !rule.include_channels.is_empty())
        && (channel_id == 0 || !rule.include_channels.contains(&channel_id))
    {
        return true;
    }
    false
}

/// Logged when `skip_moderators` is on and the guild cache cannot show whether
/// the author is a moderator. The message is still deleted. Kick and ban are not.
pub const UNKNOWN_MODERATOR_WARNING: &str =
    "moderator status unknown; deleting the message and skipping kick or ban";

/// Logged when the channel, active thread, or thread parent is not cached.
/// The message is still deleted. Kick and ban are not.
pub const UNKNOWN_MENTION_PERMISSION_WARNING: &str =
    "mention permission unknown; deleting the message and skipping kick or ban";

/// Logged when a pending `custom:` rule is gone or its config cannot be read.
/// No Discord action runs. Built-in rules do not use this stop.
pub const CUSTOM_RULE_UNAVAILABLE_WARNING: &str = "custom rule unavailable; skipping the action";

/// Missing membership cannot rule out a moderator or a skipped role.
pub const UNKNOWN_MEMBERSHIP_WARNING: &str =
    "member roles unknown; deleting the message and skipping kick or ban";

/// A channel id travels with the message, so only the moderator lookup can fail.
/// Unknown status must not be treated as permission to kick or ban.
pub fn withhold_member_penalty(skip_moderators: bool, moderator_unknown: bool) -> bool {
    skip_moderators && moderator_unknown
}

/// Spam admission. Unknown role lists still refuse when the rule has role
/// skips. Unknown moderator status is not an exemption, so the message can
/// still be deleted; kick and ban are withheld later.
pub fn spam_scope_admits(
    rule: &RuleDef,
    author_id: u64,
    channel_id: u64,
    member_roles: Option<&[u64]>,
    is_moderator: bool,
) -> bool {
    if !rule.enabled {
        return false;
    }
    if member_roles.is_none() && !rule.whitelist_roles.is_empty() {
        return false;
    }
    !rule_scope_skips(
        rule,
        author_id,
        channel_id,
        member_roles.unwrap_or(&[]),
        is_moderator,
        true,
    )
}

/// The strongest action decides which rule wins. The executor uses the
/// original action list, so this selection never changes their running order.
pub fn primary_custom_action(rule: &RuleDef) -> Option<&ActionDef> {
    rule.actions
        .iter()
        .rev()
        .max_by_key(|action| action_rank(&action.action))
}

pub fn action_rank(action: &str) -> u8 {
    match action {
        "ban" => 6,
        "kick" => 5,
        "timeout" => 4,
        "warn" => 3,
        "delete" => 2,
        _ => 1,
    }
}

/// Returns the matching role rule with the strongest action.
/// Dry-run rules are included so the executor can log them without acting.
pub fn honeypot_role_added<'a>(
    rules: &'a std::collections::HashMap<String, RuleDef>,
    added_role_ids: &[u64],
    author_id: u64,
    member_roles: &[u64],
    is_moderator: bool,
) -> Option<(&'a str, &'a RuleDef, &'a ActionDef)> {
    let mut best: Option<(&str, &RuleDef, &ActionDef)> = None;
    for (rule_id, rule) in rules {
        if !is_custom_id(rule_id) || !rule.enabled {
            continue;
        }
        if rule.params.get("kind").and_then(|v| v.as_str()) != Some("honeypot_role") {
            continue;
        }
        let Ok(role_id) = parse_role_id(rule) else {
            continue;
        };
        if !added_role_ids.contains(&role_id)
            || rule_scope_skips(rule, author_id, 0, member_roles, is_moderator, false)
        {
            continue;
        }
        let Some(action) = primary_custom_action(rule) else {
            continue;
        };
        if best.is_none_or(|(_, _, current)| {
            action_rank(&action.action) > action_rank(&current.action)
        }) {
            best = Some((rule_id, rule, action));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_rule(kind: &str, extra: Value) -> RuleDef {
        let mut params = match extra {
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                out.insert("kind".into(), json!(kind));
                for (k, v) in map {
                    out.insert(k, v);
                }
                Value::Object(out)
            }
            _ => json!({ "kind": kind }),
        };
        if !params.is_object() {
            params = json!({ "kind": kind });
        }
        RuleDef {
            name: "n".into(),
            description: String::new(),
            enabled: true,
            dry_run: true,
            params,
            actions: vec![ActionDef {
                at: 1,
                action: "kick".into(),
                duration_minutes: 0,
                delete_days: 0,
                purge_last_hour: false,
                reason: "r".into(),
            }],
            whitelist_roles: vec![],
            whitelist_channels: vec![],
            whitelist_users: vec![],
            skip_moderators: false,
            include_channels: vec![],
            log_automod: true,
            open_case: true,
        }
    }

    #[test]
    fn is_custom_id_hex_bounds() {
        assert!(is_custom_id("custom:01234567"));
        assert!(is_custom_id(&format!("custom:{}", "a".repeat(32))));
        assert!(!is_custom_id("custom:0123456"));
        assert!(!is_custom_id("custom:ABCDEF01"));
        assert!(!is_custom_id("mention_spam"));
    }

    #[test]
    fn word_token_not_substring() {
        assert!(word_rule_matches("buy scam now", &["scam".into()]));
        assert!(!word_rule_matches("scamper", &["scam".into()]));
    }

    #[test]
    fn word_literal_punctuation_boundaries() {
        for content in [
            "scam!",
            "scam.",
            "(scam)",
            "$scam",
            "scam$",
            "scam@",
            "SCAM!",
            "buy scam! now",
        ] {
            assert!(word_rule_matches(content, &["scam".into()]), "{content}");
        }
        assert!(!word_rule_matches("scamper!", &["scam".into()]));
    }

    #[test]
    fn punctuated_rule_word_keeps_normalized_behavior() {
        for (content, expected) in [
            ("sc@m", true),
            ("scam", true),
            ("sc@m!", false),
            ("sc@m.", true),
            ("scam!", false),
            ("scamper", false),
        ] {
            assert_eq!(
                word_rule_matches(content, &["sc@m".into()]),
                expected,
                "{content}"
            );
        }
    }

    #[test]
    fn rejects_lookaround_and_backref() {
        let rule = sample_rule("pattern", json!({ "pattern": "(?=a)" }));
        assert!(validate_custom_rule_shape(1, "custom:0123456789abcdef", &rule).is_err());
        let rule = sample_rule("pattern", json!({ "pattern": "(?<=a)" }));
        assert!(validate_custom_rule_shape(1, "custom:0123456789abcdef", &rule).is_err());
        let rule = sample_rule("pattern", json!({ "pattern": r"(\w)\1" }));
        assert!(validate_custom_rule_shape(1, "custom:0123456789abcdef", &rule).is_err());
    }

    #[test]
    fn unknown_moderator_withholds_kick_and_ban_only_when_skip_is_on() {
        assert!(withhold_member_penalty(true, true));
        assert!(!withhold_member_penalty(false, true));
        assert!(!withhold_member_penalty(true, false));
    }

    #[test]
    fn channel_reply_validation_and_expiry_defaults() {
        let id = "custom:01234567";
        let mut rule = sample_rule(
            "channel_content",
            json!({
                "preset": NO_LINKS_OR_MEDIA_PRESET,
                "stickers": true,
                "channel_reply": { "template": "{user} please check {channel}" }
            }),
        );
        rule.actions[0].action = "channel_reply".into();
        assert_eq!(channel_reply(&rule).unwrap().1, 10);
        assert!(validate_custom_rule_shape(1, id, &rule).is_err());
        rule.enabled = false;
        assert!(validate_custom_rule_shape(1, id, &rule).is_ok());
        rule.enabled = true;
        rule.include_channels = vec![0];
        assert!(validate_custom_rule_shape(1, id, &rule).is_err());
        rule.include_channels = vec![5];
        for (seconds, valid) in [
            (json!(0), true),
            (json!(300), true),
            (json!(301), false),
            (json!(-1), false),
            (json!(0.5), false),
            (json!(null), false),
        ] {
            rule.params["channel_reply"]["delete_after_seconds"] = seconds;
            assert_eq!(validate_custom_rule_shape(1, id, &rule).is_ok(), valid);
        }
        rule.params["channel_reply"]["delete_after_seconds"] = json!(0);
        assert_eq!(channel_reply(&rule).unwrap().1, 0);
        rule.actions.push(rule.actions[0].clone());
        assert!(validate_custom_rule_shape(1, id, &rule).is_err());
        rule.actions.pop();
        rule.params["channel_reply"]["template"] = json!(" ");
        assert!(validate_custom_rule_shape(1, id, &rule).is_err());
        rule.params["channel_reply"]["template"] = json!("😀".repeat(1001));
        assert!(validate_custom_rule_shape(1, id, &rule).is_err());
        rule.params = json!({ "kind": "honeypot_role", "role_id": 3, "channel_reply": { "template": "hello" } });
        assert!(validate_custom_rule_shape(1, id, &rule).is_err());
    }

    #[test]
    fn allowed_domains_are_bare_and_bounded() {
        let id = "custom:01234567";
        let mut rule = sample_rule("channel_content", json!({ "links": true }));
        for (domain, valid) in [
            ("EXAMPLE.test.", true),
            ("sub.example.test", true),
            ("*.example.test", false),
            ("https://example.test", false),
            ("example.test/path", false),
            ("example.test:80", false),
            ("example.test@other.test", false),
            ("example..test", false),
        ] {
            rule.params["allowed_domains"] = json!([domain]);
            assert_eq!(
                validate_custom_rule_shape(1, id, &rule).is_ok(),
                valid,
                "{domain}"
            );
        }
        rule.params["allowed_domains"] = json!(vec!["example.test"; MAX_ALLOWED_DOMAINS + 1]);
        assert!(validate_custom_rule_shape(1, id, &rule).is_err());
    }

    #[test]
    fn catastrophic_pattern_finishes_quickly() {
        let re = normalize::compile_user_regex("(a+)+b").unwrap();
        let hay = "a".repeat(50_000);
        let start = std::time::Instant::now();
        assert!(!re.is_match(&hay));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
