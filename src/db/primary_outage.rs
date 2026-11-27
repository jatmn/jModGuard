use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::time::Instant;

use crate::db_compat::{INTERACTIVE_READ_BUDGET, remote_unavailable_operation};

// A full interactive read deadline or two nearby remote faults open the gate.
const FAILURE_WINDOW: Duration = Duration::from_secs(10);
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
#[error("remote database primary is backing off after an outage")]
pub(super) struct PrimaryBackoff;

/// The caller can omit its own warning once the gate has reported the outage.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct PrimaryOutageReported(String);

impl PrimaryOutageReported {
    fn wrap(error: anyhow::Error) -> anyhow::Error {
        let message = error.to_string();
        error.context(Self(message))
    }
}

#[derive(Default)]
pub(super) struct PrimaryOutageGate(Mutex<State>);

#[derive(Default)]
struct State {
    // Work that started before a successful probe cannot reopen its old outage.
    generation: u64,
    failures: Option<Failures>,
    retry_at: Option<Instant>,
    probing: bool,
}

struct Failures {
    started: Instant,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    count: u64,
    operations: BTreeSet<&'static str>,
}

impl PrimaryOutageGate {
    #[cfg(test)]
    pub(super) fn failure_count(&self) -> u64 {
        self.0.lock().failures.as_ref().map_or(0, |f| f.count)
    }

    pub(super) fn admit(&self) -> Result<Admission<'_>> {
        let mut state = self.0.lock();
        match state.retry_at {
            None => Ok(Admission::Work(state.generation)),
            Some(retry_at) if !state.probing && Instant::now() >= retry_at => {
                state.probing = true;
                Ok(Admission::Probe(Probe { gate: self }))
            }
            Some(_) => Err(PrimaryBackoff.into()),
        }
    }

    pub(super) fn check(&self, generation: u64) -> Result<()> {
        let state = self.0.lock();
        anyhow::ensure!(
            state.retry_at.is_none() && state.generation == generation,
            PrimaryBackoff
        );
        Ok(())
    }

    pub(super) fn record_failure(
        &self,
        generation: u64,
        elapsed: Duration,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let Some(operation) = remote_unavailable_operation(&error) else {
            return error;
        };
        tracing::debug!(operation, error = ?error, "remote database operation deferred");
        let mut state = self.0.lock();
        if state.generation != generation {
            return PrimaryOutageReported::wrap(error);
        }
        let open = state.retry_at.is_some();
        let failures = state.add_failure(operation, open);
        if !open && (elapsed >= INTERACTIVE_READ_BUDGET || failures.count >= 2) {
            tracing::warn!(
                count = failures.count,
                first_failure = %failures.first,
                last_failure = %failures.last,
                operations = ?failures.operations,
                "remote database primary unavailable, backing off"
            );
            state.retry_at = Some(Instant::now() + PROBE_INTERVAL);
        }
        if state.retry_at.is_some() {
            PrimaryOutageReported::wrap(error)
        } else {
            error
        }
    }
}

impl State {
    fn add_failure(&mut self, operation: &'static str, open: bool) -> &Failures {
        let now = Instant::now();
        let timestamp = Utc::now();
        if !open
            && self
                .failures
                .as_ref()
                .is_some_and(|f| now - f.started > FAILURE_WINDOW)
        {
            self.failures = None;
        }
        let failures = self.failures.get_or_insert_with(|| Failures {
            started: now,
            first: timestamp,
            last: timestamp,
            count: 0,
            operations: BTreeSet::new(),
        });
        failures.count = failures.count.saturating_add(1);
        failures.last = timestamp;
        failures.operations.insert(operation);
        failures
    }
}

pub(super) enum Admission<'a> {
    Work(u64),
    Probe(Probe<'a>),
}

pub(super) struct Probe<'a> {
    gate: &'a PrimaryOutageGate,
}

impl Probe<'_> {
    pub(super) fn finish(self, result: Result<()>) -> Result<u64> {
        let mut state = self.gate.0.lock();
        if let Err(error) = result {
            if let Some(operation) = remote_unavailable_operation(&error) {
                state.add_failure(operation, true);
                return Err(PrimaryOutageReported::wrap(error));
            }
            return Err(error);
        }
        if let Some(failures) = state.failures.take() {
            tracing::info!(
                count = failures.count,
                first_failure = %failures.first,
                last_failure = %failures.last,
                operations = ?failures.operations,
                "remote database primary recovered"
            );
        }
        state.retry_at = None;
        state.generation = state.generation.wrapping_add(1);
        Ok(state.generation)
    }
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.0.lock();
        state.probing = false;
        if state.retry_at.is_some() {
            state.retry_at = Some(Instant::now() + PROBE_INTERVAL);
        }
    }
}
