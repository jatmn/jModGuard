//! What a launcher tells the application it starts.
//!
//! A revision-2 launcher sets `JMODGUARD_READY_FILE` and
//! `JMODGUARD_UPDATE_MAINTENANCE` and nothing that names its revision.
//! This release's launcher also sets `JMODGUARD_LAUNCHER_REVISION`.

use crate::db::REQUIRED_LAUNCHER_REVISION;

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
    fn revision_two_markers_without_a_revision_are_refused() {
        assert_eq!(refusal_revision(None, true), Some(2));
        let message = format!(
            "this release requires launcher revision {REQUIRED_LAUNCHER_REVISION}; the running launcher is revision 2. Install a new image; the in-app updater cannot upgrade to it"
        );
        assert_eq!(refusal_message_for(Some(2)), Some(message));
    }

    #[test]
    fn revision_three_and_a_standalone_process_are_not_refused() {
        assert_eq!(refusal_revision(Some("3".into()), true), Some(3));
        assert!(refusal_message_for(Some(REQUIRED_LAUNCHER_REVISION)).is_none());
        assert_eq!(refusal_revision(None, false), None);
        assert!(refusal_message_for(None).is_none());
        assert_eq!(refusal_revision(Some("nope".into()), true), Some(0));
        assert!(refusal_message_for(Some(0)).is_some());
    }
}
