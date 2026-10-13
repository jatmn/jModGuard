//! Serializes API tests that flip the global in-process readiness flag.

use std::sync::OnceLock;
use tokio::sync::{Mutex, MutexGuard};

static PROCESS_READY_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock() -> &'static Mutex<()> {
    PROCESS_READY_TEST_LOCK.get_or_init(|| Mutex::new(()))
}

/// Holds the shared lock and clears readiness on acquire and on drop.
pub struct ProcessReadyTestGuard {
    _guard: MutexGuard<'static, ()>,
}

impl ProcessReadyTestGuard {
    pub async fn acquire() -> Self {
        let guard = lock().lock().await;
        jmodguard::set_process_ready(false);
        Self { _guard: guard }
    }
}

impl Drop for ProcessReadyTestGuard {
    fn drop(&mut self) {
        jmodguard::set_process_ready(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jmodguard::{process_ready, set_process_ready};

    #[tokio::test]
    async fn guard_clears_readiness_on_scope_exit() {
        assert!(!process_ready());
        {
            let _guard = ProcessReadyTestGuard::acquire().await;
            set_process_ready(true);
            assert!(process_ready());
        }
        assert!(!process_ready());
    }

    #[tokio::test]
    async fn guard_clears_readiness_when_a_nested_task_panics() {
        assert!(!process_ready());
        let job = tokio::spawn(async {
            let _guard = ProcessReadyTestGuard::acquire().await;
            set_process_ready(true);
            assert!(process_ready());
            panic!("deliberate test panic");
        });
        assert!(job.await.is_err());
        assert!(!process_ready());
    }
}
