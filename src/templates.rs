//! Template rendering shared by log events, command responses, and plan steps.

use std::collections::BTreeMap;

/// Compact Discord elapsed time, using fixed 30-day months and 7-day weeks.
/// Leading/trailing zero units are omitted; intermediate zeros stay, so one
/// day and 38 seconds is `1d 0h 0m 38s`. `mo` distinguishes months from minutes.
pub fn format_duration(seconds: u64) -> String {
    let parts = [
        (seconds / 2_592_000, "mo"),
        ((seconds % 2_592_000) / 604_800, "w"),
        ((seconds % 2_592_000) % 604_800 / 86_400, "d"),
        ((seconds % 86_400) / 3_600, "h"),
        ((seconds % 3_600) / 60, "m"),
        (seconds % 60, "s"),
    ];
    let Some(start) = parts.iter().position(|(count, _)| *count > 0) else {
        return "0s".into();
    };
    let end = parts
        .iter()
        .rposition(|(count, _)| *count > 0)
        .unwrap_or(start);
    parts[start..=end]
        .iter()
        .map(|(count, unit)| format!("{count}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Compatibility entry point for Stats; all Discord durations share one format.
pub fn format_compact_duration(seconds: u64) -> String {
    format_duration(seconds)
}

fn release_status_icon(availability: &str) -> &'static str {
    match availability {
        "up to date" => "✅",
        "update available" => "⬆️",
        "no source update (image digest unverified)" => "⚠️",
        _ => "❓",
    }
}

fn is_ascii_hex_sha40(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn running_short_sha(item: &crate::db::ComponentUpdate) -> Option<String> {
    if item.state == "drift" {
        return None;
    }
    let sha = item.observed.as_ref().map(|v| v.source_sha.as_str())?;
    is_ascii_hex_sha40(sha).then(|| sha[..7].to_string())
}

fn release_status_reason(
    item: &crate::db::ComponentUpdate,
    component: &str,
    now: i64,
) -> Option<String> {
    if component == "webui" && item.installed.is_none() && item.desired.is_none() {
        return Some("webui not managed by updater".into());
    }
    if item.check_error.is_some() {
        return Some("check failed".into());
    }
    if item.candidate.is_none()
        || item
            .candidate
            .as_ref()
            .is_some_and(|candidate| candidate.channel != item.channel)
        || item.checked_at.is_none()
    {
        return Some("not checked".into());
    }
    if crate::db::release_check_is_stale(item.checked_at, now) {
        let hours = item
            .checked_at
            .filter(|&checked| checked <= now)
            .map(|checked| now.saturating_sub(checked) / 3600);
        return Some(match hours {
            Some(hours) => format!("check stale ({hours}h)"),
            None => "check stale".into(),
        });
    }
    None
}

fn release_status_line(
    component: &str,
    short_sha: Option<&str>,
    age_clause: Option<String>,
    tail: String,
) -> String {
    match (short_sha, age_clause) {
        (Some(sha), Some(age)) => format!("**{component}:** `{sha}` · {age} · {tail}"),
        (Some(sha), None) => format!("**{component}:** `{sha}` · {tail}"),
        (None, Some(age)) => format!("**{component}:** {age} · {tail}"),
        (None, None) => format!("**{component}:** {tail}"),
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
/// A missing case count means the guild store or membership is unavailable.
pub fn stats_response(uptime_seconds: u64, cases: Option<i64>, warns: i64, bans: i64) -> String {
    render(
        "**📊 Stats (this server)**\n\n**⏱️ Uptime:** `{uptime}`\n**📋 Cases:** `{cases}`\n**⚠️ Warns:** `{warns}`\n**🔨 Bans:** `{bans}`",
        &BTreeMap::from([
            ("uptime".into(), format_compact_duration(uptime_seconds)),
            (
                "cases".into(),
                cases.map_or_else(|| "unavailable".into(), |count| count.to_string()),
            ),
            ("warns".into(), warns.to_string()),
            ("bans".into(), bans.to_string()),
        ]),
    )
}

/// Append cached release observations without making a network request from
/// a Discord command. Release lines name missing, stale, or failed checks.
///
/// ```
/// let response: String = jmodguard::templates::stats_response_with_updates(
///     20, Some(1), 2, 3, &[], 1_000,
/// );
/// ```
pub fn stats_response_with_updates(
    uptime_seconds: u64,
    cases: Option<i64>,
    warns: i64,
    bans: i64,
    updates: &[crate::db::ComponentUpdate],
    now: i64,
) -> String {
    stats_response_with_update_availability(uptime_seconds, cases, warns, bans, Some(updates), now)
}

/// Render release lookup availability alongside moderation stats. None means
/// the lookup failed; successful reads name why a check is missing or stale.
pub fn stats_response_with_update_availability(
    uptime_seconds: u64,
    cases: Option<i64>,
    warns: i64,
    bans: i64,
    updates: Option<&[crate::db::ComponentUpdate]>,
    now: i64,
) -> String {
    let Some(updates) = updates else {
        return render(
            "{moderation}\n\n**Release status**\nunavailable",
            &BTreeMap::from([(
                "moderation".into(),
                stats_response(uptime_seconds, cases, warns, bans),
            )]),
        );
    };
    let mut lines = Vec::new();
    for component in ["bot", "api", "webui"] {
        let Some(item) = updates.iter().find(|item| item.component == component) else {
            lines.push(format!("**{component}:** not checked"));
            continue;
        };
        let observation_fresh = item
            .observed_at
            .is_some_and(|at| now.saturating_sub(at) <= 7200)
            && item.state != "drift";
        let short_sha = running_short_sha(item);
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
        let age_clause = time.filter(|_| observation_fresh).map(|time| {
            format!(
                "{} {}",
                label,
                format_compact_duration(now.saturating_sub(time).max(0) as u64)
            )
        });
        if let Some(reason) = release_status_reason(item, component, now) {
            let webui_unmanaged = reason == "webui not managed by updater";
            let sha_for_line = if webui_unmanaged {
                None
            } else {
                short_sha.as_deref()
            };
            let age_for_line = if webui_unmanaged { None } else { age_clause };
            lines.push(release_status_line(
                component,
                sha_for_line,
                age_for_line,
                reason,
            ));
            continue;
        }
        if !observation_fresh {
            let reason = if item.observed_at.is_none() {
                "unknown".into()
            } else {
                let hours = item
                    .observed_at
                    .map(|at| now.saturating_sub(at) / 3600)
                    .unwrap_or(0);
                format!("observation stale ({hours}h)")
            };
            lines.push(release_status_line(component, None, None, reason));
            continue;
        }
        let running_source = item
            .observed
            .as_ref()
            .map(|observed| observed.source_sha.as_str())
            .filter(|sha| is_ascii_hex_sha40(sha));
        let availability = if let Some(candidate) = &item.candidate {
            if item
                .installed
                .as_ref()
                .is_some_and(|installed| installed.digest == candidate.digest)
                && running_source == Some(candidate.source_sha.as_str())
            {
                "up to date"
            } else if component != "webui"
                && item.installed.is_none()
                && item
                    .observed
                    .as_ref()
                    .is_some_and(|observed| observed.source_sha == observed.base_image_sha)
                && running_source == Some(candidate.source_sha.as_str())
            {
                "no source update (image digest unverified)"
            } else if running_source.is_some_and(|sha| sha != candidate.source_sha.as_str()) {
                "update available"
            } else {
                "not checked"
            }
        } else {
            "not checked"
        };
        let tail = if availability == "not checked" {
            "not checked".to_string()
        } else {
            release_status_icon(availability).to_string()
        };
        lines.push(release_status_line(
            component,
            short_sha.as_deref(),
            age_clause,
            tail,
        ));
    }
    render(
        "{moderation}\n\n**Release status**\n{releases}",
        &BTreeMap::from([
            (
                "moderation".into(),
                stats_response(uptime_seconds, cases, warns, bans),
            ),
            ("releases".into(), lines.join("\n")),
        ]),
    )
}

const CONTAINER_USE_TEMPLATE: &str = "\
**Container use**\n\
**Bot CPU:** `{bot_cpu}`\n\
**Bot memory (real program use):** `{bot_memory}`\n\
**API CPU:** `{api_cpu}`\n\
**API memory (real program use):** `{api_memory}`";

/// Append this process's container sample to a stats reply.
///
/// `api` stays `None` when no private reading arrived. Those rows then render
/// as `unavailable`, not zero. `api_copy` labels whichever API copy answered.
/// The bot sample is a cache copy;
/// this function does not read cgroup files or wait for a CPU sample.
pub fn stats_with_container_use(
    moderation: String,
    bot: &crate::cgroup::ContainerSample,
    api: Option<&crate::cgroup::ContainerSample>,
    api_copy: Option<&str>,
) -> String {
    render(
        "{moderation}\n\n{containers}",
        &BTreeMap::from([
            ("moderation".into(), moderation),
            (
                "containers".into(),
                container_use_section(bot, api, api_copy),
            ),
        ]),
    )
}

fn container_use_section(
    bot: &crate::cgroup::ContainerSample,
    api: Option<&crate::cgroup::ContainerSample>,
    api_copy: Option<&str>,
) -> String {
    let unavailable = crate::cgroup::ContainerSample::unavailable();
    let api = api.unwrap_or(&unavailable);
    render(
        CONTAINER_USE_TEMPLATE,
        &BTreeMap::from([
            ("bot_cpu".into(), cpu_value(bot, None)),
            ("bot_memory".into(), memory_value(bot, None)),
            ("api_cpu".into(), cpu_value(api, api_copy)),
            ("api_memory".into(), memory_value(api, api_copy)),
        ]),
    )
}

fn cpu_value(sample: &crate::cgroup::ContainerSample, copy: Option<&str>) -> String {
    let base = sample
        .cpu_tenths
        .map(format_cpu_tenths)
        .unwrap_or_else(|| "unavailable".to_string());
    label_copy(base, copy)
}

fn memory_value(sample: &crate::cgroup::ContainerSample, copy: Option<&str>) -> String {
    let Some(memory) = sample.memory else {
        return label_copy("unavailable".to_string(), None);
    };
    let used = format_bytes(memory.program_bytes);
    let base = match memory.limit {
        crate::cgroup::MemoryLimit::Unlimited => format!("{used} / no limit"),
        crate::cgroup::MemoryLimit::Bytes(limit) => format!("{used} / {}", format_bytes(limit)),
    };
    label_copy(base, copy)
}

fn label_copy(value: String, copy: Option<&str>) -> String {
    if value == "unavailable" {
        return value;
    }
    match copy {
        Some(label) if !label.is_empty() => format!("{value} ({label})"),
        _ => value,
    }
}

fn format_cpu_tenths(tenths: u32) -> String {
    format!("{}.{}%", tenths / 10, tenths % 10)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes < KIB {
        return format!("{bytes} B");
    }
    let (unit, suffix) = if bytes < MIB {
        (KIB, "KiB")
    } else if bytes < GIB {
        (MIB, "MiB")
    } else {
        (GIB, "GiB")
    };
    let mut whole = bytes / unit;
    let mut frac = ((bytes % unit) * 10 + unit / 2) / unit;
    if frac >= 10 {
        whole += 1;
        frac = 0;
    }
    format!("{whole}.{frac} {suffix}")
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

/// UTF-16 units still available for one template substitution after rendering the rest.
pub fn substitution_budget(
    template: &str,
    vars: &BTreeMap<String, String>,
    key: &str,
    max_units: usize,
) -> usize {
    let mut without = vars.clone();
    without.insert(key.to_string(), String::new());
    let used = render_bounded(template, &without, max_units)
        .encode_utf16()
        .count();
    max_units.saturating_sub(used)
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
    fn compact_duration_boundaries_and_intermediate_zeros() {
        for (seconds, expected) in [
            (0, "0s"),
            (1, "1s"),
            (59, "59s"),
            (60, "1m"),
            (61, "1m 1s"),
            (216, "3m 36s"),
            (818, "13m 38s"),
            (3600, "1h"),
            (86400, "1d"),
            (86438, "1d 0h 0m 38s"),
            (259200, "3d"),
            (604800, "1w"),
            (2591999, "4w 1d 23h 59m 59s"),
            (2592000, "1mo"),
            (5184001, "2mo 0w 0d 0h 0m 1s"),
            (6062706, "2mo 1w 3d 4h 5m 6s"),
        ] {
            assert_eq!(format_duration(seconds), expected, "{seconds}");
            assert_eq!(format_compact_duration(seconds), expected, "{seconds}");
        }
    }

    #[test]
    fn screenshot_stats_preview() {
        assert_eq!(
            stats_response(
                std::time::Duration::from_secs_f64(1361.499531819).as_secs(),
                Some(0),
                0,
                1
            ),
            "**📊 Stats (this server)**\n\n**⏱️ Uptime:** `22m 41s`\n**📋 Cases:** `0`\n**⚠️ Warns:** `0`\n**🔨 Bans:** `1`"
        );
    }

    #[test]
    fn stats_distinguishes_zero_cases_from_unavailable() {
        for (cases, expected) in [(Some(0), "0"), (Some(2), "2"), (None, "unavailable")] {
            for updates in [Some(&[][..]), None] {
                let out = stats_response_with_update_availability(20, cases, 3, 4, updates, 1100);
                assert!(
                    out.contains(&format!("**📋 Cases:** `{expected}`")),
                    "{out}"
                );
                assert!(out.contains("**⚠️ Warns:** `3`"), "{out}");
                assert!(out.contains("**🔨 Bans:** `4`"), "{out}");
                assert!(!out.contains("Moderation records"), "{out}");
            }
        }
    }

    #[test]
    fn release_stats_hide_stale_observations_and_report_checked_update() {
        use crate::db::{ComponentObservation, ComponentRelease, ComponentUpdate};
        let candidate = ComponentRelease {
            upgrade: None,
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
            launcher: Some(crate::db::LauncherCompatibility {
                revision: crate::db::LAUNCHER_REVISION,
                min_revision: crate::db::REQUIRED_LAUNCHER_REVISION,
            }),
            base_abi: "debian-bookworm-amd64".into(),
        };
        let observed = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
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
        let fresh = stats_response_with_updates(20, Some(1), 2, 3, &[row.clone()], 1100);
        assert!(fresh.contains("`aaaaaaa`"));
        assert!(fresh.contains("⬆️"));
        assert!(!fresh.contains("update available"));
        row.channel = "nightly".into();
        let wrong_channel = stats_response_with_updates(20, Some(1), 2, 3, &[row.clone()], 1100);
        assert!(!wrong_channel.contains("update available"));
        let wrong_api = wrong_channel
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(wrong_api.contains("not checked"), "{wrong_api}");
        assert!(!wrong_api.contains('❓'), "{wrong_api}");
        assert!(!wrong_api.contains("⬆️"), "{wrong_api}");
        assert!(wrong_channel.contains("**bot:** not checked"));
        assert!(wrong_channel.contains("**webui:** not checked"));
        row.channel = "stable".into();
        row.candidate.as_mut().unwrap().source_sha = "a".repeat(40);
        row.observed.as_mut().unwrap().base_image_sha = "a".repeat(40);
        let baked_current = stats_response_with_updates(20, Some(1), 2, 3, &[row.clone()], 1100);
        let api_line = baked_current
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api_line.ends_with("⚠️"), "{api_line}");
        assert!(!baked_current.contains("no source update"));
        row.channel = "nightly".into();
        let wrong_channel_current =
            stats_response_with_updates(20, Some(1), 2, 3, &[row.clone()], 1100);
        assert!(!wrong_channel_current.contains("no source update"));
        row.channel = "stable".into();
        // Source comparisons still use the full SHA, even when prefixes match.
        let mut same_prefix = row.clone();
        same_prefix.candidate.as_mut().unwrap().source_sha = format!("aaaaaaa{}", "b".repeat(33));
        let out = stats_response_with_updates(20, Some(1), 2, 3, &[same_prefix], 1100);
        assert!(
            out.lines()
                .any(|line| line.starts_with("**api:**") && line.ends_with("⬆️"))
        );

        for component in ["bot", "api", "webui"] {
            for (source, expected) in [
                ("abcdef0123456789abcdef0123456789abcdef01", "abcdef0"),
                ("ABCDEF0123456789ABCDEF0123456789ABCDEF01", "ABCDEF0"),
            ] {
                let mut display = row.clone();
                display.component = component.into();
                display.observed.as_mut().unwrap().source_sha = source.into();
                display.candidate.as_mut().unwrap().source_sha = source.into();
                display.installed = display.candidate.clone();
                let out = stats_response_with_updates(20, Some(1), 2, 3, &[display], 1100);
                let line = out
                    .lines()
                    .find(|line| line.starts_with(&format!("**{component}:**")))
                    .unwrap();
                assert!(line.contains(&format!("`{expected}`")), "{line}");
                assert!(line.ends_with("✅"), "{line}");
            }
        }
        row.observed_at = Some(1);
        let stale = stats_response_with_updates(20, Some(1), 2, 3, &[row], 9000);
        assert!(!stale.contains(&"a".repeat(40)));
        let stale_api = stale
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(stale_api.contains("check stale (2h)"), "{stale_api}");
        assert!(stale_api.contains("`aaaaaaa`"), "{stale_api}");
        assert!(!stale_api.contains('❓'), "{stale_api}");
        assert!(!stale_api.contains("unknown"), "{stale_api}");
    }

    #[test]
    fn missing_release_rows_use_the_unknown_icon() {
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[], 1_000);
        for component in ["bot", "api", "webui"] {
            assert_eq!(
                out.lines()
                    .find(|line| line.starts_with(&format!("**{component}:**")))
                    .unwrap(),
                &format!("**{component}:** not checked"),
                "{component} missing from {out}"
            );
        }
        assert!(!out.contains("**bot:** unknown"));
    }

    fn sample_api_update_row(
        candidate: Option<crate::db::ComponentRelease>,
        checked_at: Option<i64>,
        check_error: Option<String>,
        observed: Option<crate::db::ComponentObservation>,
        observed_at: Option<i64>,
        channel: &str,
    ) -> crate::db::ComponentUpdate {
        use crate::db::ComponentUpdate;
        ComponentUpdate {
            component: "api".into(),
            channel: channel.into(),
            cadence: "off".into(),
            candidate,
            checked_at,
            check_error,
            desired: None,
            desired_launcher: None,
            installed: None,
            installed_launcher: None,
            launcher_sha256: None,
            launcher_installed_at: None,
            previous: None,
            observed,
            observed_at,
            installed_at: None,
            state: "running".into(),
            last_error: None,
        }
    }

    fn sample_observed(source_sha: &str) -> crate::db::ComponentObservation {
        use crate::db::ComponentObservation;
        ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            source_sha: source_sha.into(),
            executable_sha256: "c".repeat(64),
            admin_sha256: None,
            base_image_sha: "d".repeat(40),
            started_at: 900,
            min_schema: 15,
            max_schema: 15,
            api_capability: 1,
            self_update_enabled: false,
            base_abi: "debian-bookworm-amd64".into(),
        }
    }

    fn sample_candidate(
        channel: &str,
        source_sha: &str,
        digest_hex: &str,
    ) -> crate::db::ComponentRelease {
        use crate::db::ComponentRelease;
        ComponentRelease {
            upgrade: None,
            component: "api".into(),
            channel: channel.into(),
            release_id: "v1".into(),
            source_sha: source_sha.into(),
            published_at: "2026-09-23T00:00:00Z".into(),
            image: "ghcr.io/jatmn/jmodguard-api".into(),
            digest: format!("sha256:{digest_hex}"),
            min_schema: 15,
            max_schema: 15,
            result_schema: 15,
            min_api: 1,
            max_api: 1,
            launcher_protocol: 1,
            launcher: None,
            base_abi: "debian-bookworm-amd64".into(),
        }
    }

    #[test]
    fn status_says_not_checked_when_there_is_no_candidate() {
        let source = "abcdef0123456789abcdef0123456789abcdef01";
        let observed = sample_observed(source);
        let row = sample_api_update_row(None, None, None, Some(observed), Some(1_000), "latest");
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[row], 1_100);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("`abcdef0`"), "{api}");
        assert!(api.contains("not checked"), "{api}");
        assert!(!api.contains('❓'), "{api}");

        let candidate = sample_candidate("stable", source, &"b".repeat(64));
        let row = sample_api_update_row(
            Some(candidate),
            Some(1_100),
            None,
            Some(sample_observed(source)),
            Some(1_000),
            "latest",
        );
        let mismatch = stats_response_with_updates(0, Some(0), 0, 0, &[row], 1_200);
        let api = mismatch
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("not checked"), "{api}");
        assert!(!api.contains("⬆️"), "{api}");
        assert!(!api.contains("update available"), "{api}");

        let missing = stats_response_with_updates(0, Some(0), 0, 0, &[], 1_000);
        for component in ["bot", "api", "webui"] {
            assert_eq!(
                missing
                    .lines()
                    .find(|line| line.starts_with(&format!("**{component}:**")))
                    .unwrap(),
                &format!("**{component}:** not checked")
            );
        }
    }

    #[test]
    fn status_says_check_stale_with_hour_age() {
        let source = "abcdef0123456789abcdef0123456789abcdef01";
        let candidate = sample_candidate("latest", source, &"b".repeat(64));
        let row = sample_api_update_row(
            Some(candidate),
            Some(7_201),
            None,
            Some(sample_observed(source)),
            Some(1_000),
            "latest",
        );
        let now = 14_402;
        let out = stats_response_with_updates(0, Some(0), 0, 0, std::slice::from_ref(&row), now);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("`abcdef0`"), "{api}");
        assert!(api.contains("check stale (2h)"), "{api}");
        assert!(!api.contains('❓'), "{api}");

        let mut fresh_boundary = sample_api_update_row(
            Some(sample_candidate("latest", source, &"b".repeat(64))),
            Some(now - 7_199),
            None,
            Some(sample_observed(source)),
            Some(now - 3_600),
            "latest",
        );
        fresh_boundary.installed = fresh_boundary.candidate.clone();
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[fresh_boundary], now);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.ends_with("⬆️") || api.ends_with('✅'), "{api}");
        assert!(!api.contains("check stale"), "{api}");

        let row = sample_api_update_row(
            Some(sample_candidate("latest", source, &"b".repeat(64))),
            Some(now - 3 * 3600),
            None,
            Some(sample_observed(source)),
            Some(1_000),
            "latest",
        );
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[row], now);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("check stale (3h)"), "{api}");

        let boundary_now = 14_400;
        let mut fresh = sample_api_update_row(
            Some(sample_candidate("latest", source, &"b".repeat(64))),
            Some(boundary_now - 7_200),
            None,
            Some(sample_observed(source)),
            Some(1_000),
            "latest",
        );
        fresh.installed = fresh.candidate.clone();
        fresh.observed_at = Some(boundary_now - 3_600);
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[fresh], boundary_now);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("check stale (2h)"), "{api}");
        assert!(!api.ends_with('✅'), "{api}");

        let mut future_checked = sample_api_update_row(
            Some(sample_candidate("latest", source, &"b".repeat(64))),
            Some(boundary_now + 60),
            None,
            Some(sample_observed(source)),
            Some(1_000),
            "latest",
        );
        future_checked.installed = future_checked.candidate.clone();
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[future_checked], boundary_now);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("check stale"), "{api}");
        assert!(!api.contains("check stale (0h)"), "{api}");
        assert!(!api.ends_with('✅'), "{api}");
    }

    #[test]
    fn status_says_check_failed_when_check_error_is_set() {
        let source = "abcdef0123456789abcdef0123456789abcdef01";
        let row = sample_api_update_row(
            Some(sample_candidate("latest", source, &"b".repeat(64))),
            Some(1_100),
            Some("registry returned HTTP 429".into()),
            Some(sample_observed(source)),
            Some(1_000),
            "latest",
        );
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[row], 1_200);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("`abcdef0`"), "{api}");
        assert!(api.contains("check failed"), "{api}");
        assert!(!api.contains('✅'), "{api}");
        assert!(!api.contains("⬆️"), "{api}");
        assert!(!api.contains('❓'), "{api}");
    }

    #[test]
    fn status_says_webui_is_not_managed_when_no_release_is_installed() {
        use crate::db::{ComponentRelease, ComponentUpdate};
        let source = "abcdef0123456789abcdef0123456789abcdef01";
        let candidate = ComponentRelease {
            upgrade: None,
            component: "webui".into(),
            channel: "latest".into(),
            release_id: "v1".into(),
            source_sha: source.into(),
            published_at: "2026-09-23T00:00:00Z".into(),
            image: "ghcr.io/jatmn/jmodguard-webui".into(),
            digest: format!("sha256:{}", "b".repeat(64)),
            min_schema: 15,
            max_schema: 15,
            result_schema: 15,
            min_api: 1,
            max_api: 1,
            launcher_protocol: 1,
            launcher: None,
            base_abi: "static-webui-v1".into(),
        };
        for candidate in [Some(candidate.clone()), None] {
            let row = ComponentUpdate {
                component: "webui".into(),
                channel: "latest".into(),
                cadence: "off".into(),
                candidate: candidate.clone(),
                checked_at: Some(1_100),
                check_error: None,
                desired: None,
                desired_launcher: None,
                installed: None,
                installed_launcher: None,
                launcher_sha256: None,
                launcher_installed_at: None,
                previous: None,
                observed: Some(sample_observed(source)),
                observed_at: Some(1_000),
                installed_at: None,
                state: "running".into(),
                last_error: None,
            };
            let out = stats_response_with_updates(0, Some(0), 0, 0, &[row], 1_200);
            let webui = out
                .lines()
                .find(|line| line.starts_with("**webui:**"))
                .unwrap();
            assert!(webui.contains("webui not managed by updater"), "{webui}");
            assert!(!webui.contains('❓'), "{webui}");
            assert!(!webui.contains("first seen"), "{webui}");
            assert!(!webui.contains("unknown"), "{webui}");
        }

        let mut queued = ComponentUpdate {
            component: "webui".into(),
            channel: "latest".into(),
            cadence: "off".into(),
            candidate: Some(candidate.clone()),
            checked_at: Some(1_100),
            check_error: None,
            desired: Some(candidate.clone()),
            desired_launcher: None,
            installed: None,
            installed_launcher: None,
            launcher_sha256: None,
            launcher_installed_at: None,
            previous: None,
            observed: Some(sample_observed(source)),
            observed_at: Some(1_000),
            installed_at: None,
            state: "queued".into(),
            last_error: None,
        };
        queued.candidate.as_mut().unwrap().source_sha = "c".repeat(40);
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[queued], 1_200);
        let webui = out
            .lines()
            .find(|line| line.starts_with("**webui:**"))
            .unwrap();
        assert!(webui.ends_with("⬆️"), "{webui}");
        assert!(!webui.contains("webui not managed by updater"), "{webui}");

        let mut api = sample_api_update_row(
            Some(sample_candidate("latest", source, &"b".repeat(64))),
            Some(1_100),
            None,
            Some({
                let mut observed = sample_observed(source);
                observed.base_image_sha = source.into();
                observed
            }),
            Some(1_000),
            "latest",
        );
        api.component = "api".into();
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[api], 1_200);
        let api_line = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api_line.ends_with("⚠️"), "{api_line}");
    }

    #[test]
    fn status_wont_claim_up_to_date_from_stale_observation_with_fresh_check() {
        let running = "a".repeat(40);
        let candidate_source = "b".repeat(40);
        let candidate = sample_candidate("latest", &candidate_source, &"c".repeat(64));
        let mut row = sample_api_update_row(
            Some(candidate.clone()),
            Some(8_900),
            None,
            Some(sample_observed(&running)),
            Some(1_000),
            "latest",
        );
        row.installed = Some(candidate);
        row.observed.as_mut().unwrap().source_sha = candidate_source;
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[row], 9_000);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("observation stale (2h)"), "{api}");
        assert!(!api.ends_with('✅'), "{api}");
        assert!(!api.contains("⬆️"), "{api}");
    }

    #[test]
    fn status_shows_the_running_short_sha_after_a_fresh_check() {
        let source = "abcdef0123456789abcdef0123456789abcdef01";
        let candidate = sample_candidate("latest", source, &"b".repeat(64));
        let mut row = sample_api_update_row(
            Some(candidate.clone()),
            Some(1_100),
            None,
            Some(sample_observed(source)),
            Some(1_000),
            "latest",
        );
        row.installed = Some(candidate);
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[row], 1_200);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("`abcdef0`"), "{api}");
        assert!(api.ends_with('✅'), "{api}");
        assert!(!api.contains("not checked"), "{api}");
        assert!(!api.contains("check stale"), "{api}");
        assert!(!api.contains("check failed"), "{api}");
        assert!(!api.contains('❓'), "{api}");

        let running = "abcdef0123456789abcdef0123456789abcdef00";
        let row = sample_api_update_row(
            Some(sample_candidate("latest", source, &"b".repeat(64))),
            Some(1_100),
            None,
            Some(sample_observed(running)),
            Some(1_000),
            "latest",
        );
        let out = stats_response_with_updates(0, Some(0), 0, 0, &[row], 1_200);
        let api = out
            .lines()
            .find(|line| line.starts_with("**api:**"))
            .unwrap();
        assert!(api.contains("`abcdef0`"), "{api}");
        assert!(!api.contains("`abcdef1`"), "{api}");
        assert!(api.ends_with("⬆️"), "{api}");
    }

    #[test]
    fn status_does_not_wait_for_a_cpu_sample() {
        let started = std::time::Instant::now();
        let text = stats_with_container_use(
            "**📊 Stats (this server)**".into(),
            &crate::cgroup::ContainerSample::unavailable(),
            None,
            None,
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert!(text.contains("**Bot CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**Bot memory (real program use):** `unavailable`"),
            "{text}"
        );
        assert!(text.contains("**API CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**API memory (real program use):** `unavailable`"),
            "{text}"
        );
        assert!(!text.contains("0.0%"), "{text}");
        assert!(!text.contains("0 B"), "{text}");
    }

    #[test]
    fn container_use_shows_program_memory_and_leaves_the_api_unavailable() {
        let bot = crate::cgroup::ContainerSample {
            memory: Some(crate::cgroup::MemoryUse {
                program_bytes: 900,
                limit: crate::cgroup::MemoryLimit::Unlimited,
            }),
            cpu_tenths: Some(250),
        };
        let text = stats_with_container_use("stats".into(), &bot, None, None);
        assert!(text.contains("**Bot CPU:** `25.0%`"), "{text}");
        assert!(
            text.contains("**Bot memory (real program use):** `900 B / no limit`"),
            "{text}"
        );
        assert!(text.contains("**API CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**API memory (real program use):** `unavailable`"),
            "{text}"
        );
    }

    #[test]
    fn container_memory_limit_renders_without_a_cpu_sample() {
        let bot = crate::cgroup::ContainerSample {
            memory: Some(crate::cgroup::MemoryUse {
                program_bytes: 4096,
                limit: crate::cgroup::MemoryLimit::Bytes(8192),
            }),
            cpu_tenths: None,
        };
        let text = stats_with_container_use("stats".into(), &bot, None, None);
        assert!(text.contains("**Bot CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**Bot memory (real program use):** `4.0 KiB / 8.0 KiB`"),
            "{text}"
        );
    }

    #[test]
    fn unreadable_cgroup_layout_renders_unavailable() {
        let dir = std::env::temp_dir().join(format!(
            "jmg-status-cgroup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meminfo"), "MemTotal: 1\n").unwrap();
        std::fs::write(dir.join("stat"), "cpu 1 2 3\n").unwrap();
        let reading = crate::cgroup::read_cgroup(Some(&dir), Some(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(reading.memory.is_none());
        assert!(reading.cpu.is_none());
        let sample = crate::cgroup::ContainerSample {
            memory: reading.memory,
            cpu_tenths: None,
        };
        let text = stats_with_container_use("stats".into(), &sample, None, None);
        assert!(text.contains("**Bot CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**Bot memory (real program use):** `unavailable`"),
            "{text}"
        );
        assert!(!text.contains("0.0%"), "{text}");
        assert!(!text.contains("0 B"), "{text}");
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
