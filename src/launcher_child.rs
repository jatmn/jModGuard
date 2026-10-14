//! What a launcher tells the application it starts.
//!
//! A revision-2 launcher sets `JMODGUARD_READY_FILE` and
//! `JMODGUARD_UPDATE_MAINTENANCE` and nothing that names its revision.
//! This release's launcher also sets `JMODGUARD_LAUNCHER_REVISION`.

use crate::db::REQUIRED_LAUNCHER_REVISION;

/// Fatal Discord HTTP 401 at startup or gateway close 4004 uses this code.
/// Launchers at [`DISCORD_TOKEN_HOLD_LAUNCHER_REVISION`] or newer hold this child
/// with a long fallback retry (30 minutes, doubling to six hours). Under older
/// launchers application admission refuses startup. Fixing the token and restarting
/// resets the hold. Standalone bots still exit with this code. Keep distinct from
/// lost-lease/crash exit 1 and launcher handoff 77.
pub const DISCORD_TOKEN_REJECTED_EXIT_CODE: i32 = 78;

/// First launcher revision that holds fatal Discord authentication exits.
pub const DISCORD_TOKEN_HOLD_LAUNCHER_REVISION: u32 = 7;

/// Whether exiting on token rejection is safe from an old launcher's restart loop.
/// Pass [`detected_launcher_revision`]'s result; standalone processes may exit too.
pub fn launcher_holds_auth_exit(revision: Option<u32>) -> bool {
    revision.is_none_or(|revision| revision >= DISCORD_TOKEN_HOLD_LAUNCHER_REVISION)
}

/// `None` when this process was not started by a launcher.
/// A present value is the launcher revision to compare with
/// [`REQUIRED_LAUNCHER_REVISION`].
pub fn detected_launcher_revision() -> Option<u32> {
    refusal_revision(
        std::env::var("JMODGUARD_LAUNCHER_REVISION").ok(),
        std::env::var_os("JMODGUARD_READY_FILE").is_some()
            || std::env::var_os("JMODGUARD_UPDATE_MAINTENANCE").is_some(),
    )
}

/// Message to log when this process must idle. Health stays up; the process
/// must not exit, claim the writer lease, or moderate.
pub fn refusal_message() -> Option<String> {
    refusal_message_for(detected_launcher_revision())
}

fn refusal_message_for(revision: Option<u32>) -> Option<String> {
    let revision = revision?;
    if revision >= REQUIRED_LAUNCHER_REVISION {
        return None;
    }
    Some(format!(
        "this release requires launcher revision {REQUIRED_LAUNCHER_REVISION}; the running launcher is revision {revision}. Install a new image; the in-app updater cannot upgrade to it"
    ))
}

fn refusal_revision(revision: Option<String>, launcher_markers: bool) -> Option<u32> {
    if let Some(raw) = revision {
        return Some(raw.trim().parse::<u32>().unwrap_or(0));
    }
    launcher_markers.then_some(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_exit_requires_revision_seven_or_no_launcher() {
        for revision in 0..=6 {
            assert!(!launcher_holds_auth_exit(Some(revision)));
        }
        for revision in [7, 8, u32::MAX] {
            assert!(launcher_holds_auth_exit(Some(revision)));
        }
        assert!(launcher_holds_auth_exit(refusal_revision(None, false)));
        assert!(!launcher_holds_auth_exit(refusal_revision(None, true)));
        for raw in ["nope", "", "-1", "4294967296"] {
            let revision = refusal_revision(Some(raw.into()), true);
            assert_eq!(revision, Some(0));
            assert!(!launcher_holds_auth_exit(revision));
        }
    }

    #[test]
    fn revision_two_markers_without_a_revision_are_refused() {
        assert_eq!(refusal_revision(None, true), Some(2));
        let message = format!(
            "this release requires launcher revision {REQUIRED_LAUNCHER_REVISION}; the running launcher is revision 2. Install a new image; the in-app updater cannot upgrade to it"
        );
        assert_eq!(refusal_message_for(Some(2)), Some(message));
    }

    #[test]
    fn admitted_launchers_hold_auth_exits_and_standalone_is_not_refused() {
        assert_eq!(refusal_revision(Some("3".into()), true), Some(3));
        for revision in 0..7 {
            assert!(refusal_message_for(Some(revision)).is_some());
        }
        for revision in 7..=crate::db::LAUNCHER_REVISION {
            assert!(refusal_message_for(Some(revision)).is_none());
            assert!(launcher_holds_auth_exit(Some(revision)));
        }
        assert_eq!(refusal_revision(None, false), None);
        assert!(refusal_message_for(None).is_none());
        assert_eq!(refusal_revision(Some("nope".into()), true), Some(0));
        assert!(refusal_message_for(Some(0)).is_some());
    }
}
