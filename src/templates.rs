//! Template rendering shared by log events, command responses, and plan steps.

use std::collections::BTreeMap;

/// Human-readable elapsed time: fixed 30-day months, then weeks down to seconds.
pub fn format_duration(mut seconds: u64) -> String {
    let mut parts = Vec::new();
    for (unit, size) in [
        ("month", 30 * 86400),
        ("week", 7 * 86400),
        ("day", 86400),
        ("hour", 3600),
        ("minute", 60),
        ("second", 1),
    ] {
        let count = seconds / size;
        seconds %= size;
        if count > 0 {
            let plural = if count == 1 { "" } else { "s" };
            parts.push(format!("{count} {unit}{plural}"));
        }
    }
    if parts.is_empty() {
        "0 seconds".into()
    } else {
        parts.join(", ")
    }
}

/// Absolute UTC timestamp for Discord display, without fractional-second noise.
pub fn format_timestamp(time: chrono::DateTime<chrono::Utc>) -> String {
    time.format("%b %d, %Y • %H:%M:%S UTC").to_string()
}

/// Format known timestamp variables only at the presentation boundary.
/// Raw event timestamps remain available to audit correlation before this call.
pub fn format_log_timestamps(vars: &mut BTreeMap<String, String>) {
    for key in ["time", "account_created", "joined"] {
        if let Some(value) = vars.get_mut(key)
            && let Ok(time) = chrono::DateTime::parse_from_rfc3339(value)
        {
            *value = format_timestamp(time.with_timezone(&chrono::Utc));
        }
    }
}

/// Default stats response, rendered through the same template engine as logs.
pub fn stats_response(uptime_seconds: u64, records: i64, warns: i64, bans: i64) -> String {
    render(
        "**📊 Stats (this server)**\n\n**Uptime:** `{uptime}`\n**Moderation records:** `{records}`\n**Warns:** `{warns}`\n**Bans:** `{bans}`",
        &BTreeMap::from([
            ("uptime".into(), format_duration(uptime_seconds)),
            ("records".into(), records.to_string()),
            ("warns".into(), warns.to_string()),
            ("bans".into(), bans.to_string()),
        ]),
    )
}

/// Append cached release observations without making a network request from
/// a Discord command. Unknown includes stale checks and legacy manual images.
pub fn stats_response_with_updates(
    uptime_seconds: u64,
    records: i64,
    warns: i64,
    bans: i64,
    updates: &[crate::db::ComponentUpdate],
    now: i64,
) -> String {
    let mut lines = Vec::new();
    for component in ["bot", "api", "webui"] {
        let Some(item) = updates.iter().find(|item| item.component == component) else {
            lines.push(format!("**{component}:** unknown"));
            continue;
        };
        let fresh = item
            .observed_at
            .is_some_and(|at| now.saturating_sub(at) <= 7200)
            && item.state != "drift";
        let sha = if fresh {
            item.observed
                .as_ref()
                .map(|v| v.source_sha.as_str())
                .unwrap_or("unknown")
        } else {
            "unknown"
        };
        let (time, label) = match (
            item.installed_at,
            item.installed.as_ref(),
            item.observed.as_ref(),
        ) {
            (Some(time), Some(installed), Some(observed))
                if installed.source_sha == observed.source_sha =>
            {
                (Some(time), "installed")
            }
            _ => (item.observed.as_ref().map(|v| v.started_at), "first seen"),
        };
        let age = time
            .filter(|_| fresh)
            .map(|time| {
                format!(
                    "{} ago",
                    format_duration(now.saturating_sub(time).max(0) as u64)
                )
            })
            .unwrap_or_else(|| "unknown".into());
        let availability = if !fresh
            || item.check_error.is_some()
            || item
                .checked_at
                .is_none_or(|checked| now.saturating_sub(checked) > 7200)
            || item
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.channel != item.channel)
        {
            "unknown"
        } else if let Some(candidate) = &item.candidate {
            if item
                .installed
                .as_ref()
                .is_some_and(|installed| installed.digest == candidate.digest)
                && sha == candidate.source_sha
            {
                "up to date"
            } else if component != "webui"
                && item.installed.is_none()
                && item
                    .observed
                    .as_ref()
                    .is_some_and(|observed| observed.source_sha == observed.base_image_sha)
                && sha == candidate.source_sha
            {
                "no source update (image digest unverified)"
            } else if sha != "unknown" && sha != candidate.source_sha {
                "update available"
            } else {
                "unknown"
            }
        } else {
            "unknown"
        };
        lines.push(format!(
            "**{component}:** `{sha}` · {label} {age} · {availability}"
        ));
    }
    render(
        "{moderation}\n\n**Release status**\n{releases}",
        &BTreeMap::from([
            (
                "moderation".into(),
                stats_response(uptime_seconds, records, warns, bans),
            ),
            ("releases".into(), lines.join("\n")),
        ]),
    )
}

/// Renders a template, replacing `{variable}` tokens with values from `vars`.
/// Unknown tokens are left untouched so template mistakes are visible.
pub fn render(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{'
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '}')
        {
            let key: String = chars[i + 1..i + 1 + end].iter().collect();
            if let Some(v) = vars.get(&key) {
                out.push_str(v);
                i += end + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Render a Discord-sized report without first allocating an expanded message.
/// The limit counts UTF-16 units; unknown tokens retain the normal renderer's behavior.
pub fn render_bounded(template: &str, vars: &BTreeMap<String, String>, max_units: usize) -> String {
    let mut out = String::new();
    let mut remaining = max_units;
    let mut append = |text: &str| {
        for ch in text.chars() {
            if ch.len_utf16() > remaining {
                return false;
            }
            out.push(ch);
            remaining -= ch.len_utf16();
        }
        true
    };
    for chunk in template.split_inclusive('}') {
        let complete = if chunk.ends_with('}') {
            if let Some(start) = chunk.rfind('{') {
                if let Some(value) = vars.get(&chunk[start + 1..chunk.len() - 1]) {
                    append(&chunk[..start]) && append(value)
                } else {
                    append(chunk)
                }
            } else {
                append(chunk)
            }
        } else {
            append(chunk)
        };
        if !complete {
            if max_units > 0 {
                // Make room for the marker even if the previous character used two units.
                if remaining == 0 {
                    out.pop();
                }
                out.push('…');
            }
            break;
        }
    }
    out
}

/// Builds the standard variable map for a user.
pub fn user_vars(user_id: u64, username: &str, display_name: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("user".into(), display_name.to_string());
    m.insert("user_id".into(), user_id.to_string());
    m.insert("mention".into(), format!("<@{}>", user_id));
    m.insert("username".into(), username.to_string());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_known_and_keeps_unknown() {
        let mut vars = BTreeMap::new();
        vars.insert("user".into(), "alice".into());
        let out = render("hi {user}, {missing}!", &vars);
        assert_eq!(out, "hi alice, {missing}!");
    }

    #[test]
    fn nested_braces_ignored() {
        let out = render("literal {", &BTreeMap::new());
        assert_eq!(out, "literal {");
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    #[test]
    fn bounded_render_preserves_template_semantics() {
        let vars = BTreeMap::from([("user".into(), "alice {unknown}".into())]);
        for template in [
            "hi {user}",
            "{{user}}",
            "{unknown} {user}",
            "literal {",
            "{outer {user} tail",
        ] {
            assert_eq!(
                render_bounded(template, &vars, 2000),
                render(template, &vars)
            );
        }
    }
    #[test]
    fn oversized_substitutions_stop_at_unicode_budget() {
        let vars = BTreeMap::from([("body".into(), "🦀".repeat(10_000))]);
        for limit in [0, 1, 2, 3, 1999, 2000] {
            let out = render_bounded("{body}{body}{body}", &vars, limit);
            assert!(out.encode_utf16().count() <= limit);
            if limit > 0 {
                assert!(out.ends_with('…'));
            }
        }
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn readable_elapsed_time_boundaries() {
        for (seconds, expected) in [
            (0, "0 seconds"),
            (1, "1 second"),
            (59, "59 seconds"),
            (60, "1 minute"),
            (61, "1 minute, 1 second"),
            (3600, "1 hour"),
            (86400, "1 day"),
            (604800, "1 week"),
            (2591999, "4 weeks, 1 day, 23 hours, 59 minutes, 59 seconds"),
            (2592000, "1 month"),
            (
                6062706,
                "2 months, 1 week, 3 days, 4 hours, 5 minutes, 6 seconds",
            ),
        ] {
            assert_eq!(format_duration(seconds), expected, "{seconds}");
        }
    }

    #[test]
    fn screenshot_stats_preview() {
        assert_eq!(
            stats_response(
                std::time::Duration::from_secs_f64(1361.499531819).as_secs(),
                0,
                0,
                1
            ),
            "**📊 Stats (this server)**\n\n**Uptime:** `22 minutes, 41 seconds`\n**Moderation records:** `0`\n**Warns:** `0`\n**Bans:** `1`"
        );
    }

    #[test]
    fn release_stats_hide_stale_observations_and_report_checked_update() {
        use crate::db::{ComponentObservation, ComponentRelease, ComponentUpdate};
        let candidate = ComponentRelease {
            component: "api".into(),
            channel: "stable".into(),
            release_id: "v1".into(),
            source_sha: "b".repeat(40),
            published_at: "2026-09-23T00:00:00Z".into(),
            image: "ghcr.io/jatmn/jmodguard-api".into(),
            digest: format!("sha256:{}", "b".repeat(64)),
            min_schema: 15,
            max_schema: 15,
            result_schema: 15,
            min_api: 1,
            max_api: 1,
            launcher_protocol: 1,
            base_abi: "debian-bookworm-amd64".into(),
        };
        let observed = ComponentObservation {
            source_sha: "a".repeat(40),
            executable_sha256: "c".repeat(64),
            admin_sha256: None,
            base_image_sha: "d".repeat(40),
            started_at: 900,
            min_schema: 15,
            max_schema: 15,
            api_capability: 1,
            self_update_enabled: false,
            base_abi: "debian-bookworm-amd64".into(),
        };
        let mut row = ComponentUpdate {
            component: "api".into(),
            channel: "stable".into(),
            cadence: "off".into(),
            candidate: Some(candidate),
            checked_at: Some(1000),
            check_error: None,
            desired: None,
            desired_launcher: None,
            installed: None,
            installed_launcher: None,
            launcher_sha256: None,
            launcher_installed_at: None,
            previous: None,
            observed: Some(observed),
            observed_at: Some(1000),
            installed_at: None,
            state: "running".into(),
            last_error: None,
        };
        let fresh = stats_response_with_updates(20, 1, 2, 3, &[row.clone()], 1100);
        assert!(fresh.contains(&format!("`{}`", "a".repeat(40))));
        assert!(fresh.contains("update available"));
        row.channel = "nightly".into();
        let wrong_channel = stats_response_with_updates(20, 1, 2, 3, &[row.clone()], 1100);
        assert!(!wrong_channel.contains("update available"));
        assert!(wrong_channel.contains("unknown"));
        row.channel = "stable".into();
        row.candidate.as_mut().unwrap().source_sha = "a".repeat(40);
        row.observed.as_mut().unwrap().base_image_sha = "a".repeat(40);
        let baked_current = stats_response_with_updates(20, 1, 2, 3, &[row.clone()], 1100);
        assert!(baked_current.contains("no source update (image digest unverified)"));
        row.channel = "nightly".into();
        let wrong_channel_current = stats_response_with_updates(20, 1, 2, 3, &[row.clone()], 1100);
        assert!(!wrong_channel_current.contains("no source update"));
        row.channel = "stable".into();
        row.observed_at = Some(1);
        let stale = stats_response_with_updates(20, 1, 2, 3, &[row], 9000);
        assert!(!stale.contains(&"a".repeat(40)));
        assert!(stale.contains("unknown"));
    }

    #[test]
    fn log_dates_preserve_the_instant_and_do_not_rewrite_content() {
        let raw = "2026-09-22T11:02:22.758612736-07:00";
        let mut vars = BTreeMap::from([
            ("time".into(), raw.into()),
            ("account_created".into(), raw.into()),
            ("joined".into(), "Unavailable".into()),
            ("content".into(), raw.into()),
        ]);
        format_log_timestamps(&mut vars);
        assert_eq!(vars["time"], "Sep 22, 2026 • 18:02:22 UTC");
        assert_eq!(vars["account_created"], vars["time"]);
        assert_eq!(vars["joined"], "Unavailable");
        assert_eq!(vars["content"], raw);
        vars.insert("time".into(), "operator-defined time".into());
        format_log_timestamps(&mut vars);
        assert_eq!(vars["time"], "operator-defined time");
    }
}
