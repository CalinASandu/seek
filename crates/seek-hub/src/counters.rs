//! What an attacker (or a broken PC) can cause, counted for `status` and logged at most once a minute per kind
//! so the log cannot be flooded.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use seek_sync::Refusal;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use uuid::Uuid;

const LOG_EVERY: Duration = Duration::from_secs(60);

/// Something an attacker (or a broken PC) can cause; counted and logged at most once a minute per kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    RefusedHandshake,
    PairingFailure,
    OwnershipViolation,
    /// `QuotaFull`, `HubFull` or `TooManyDevices`.
    QuotaHit,
}

impl Event {
    fn label(self) -> &'static str {
        match self {
            Event::RefusedHandshake => "refused connection",
            Event::PairingFailure => "failed pairing attempt",
            Event::OwnershipViolation => "ownership violation",
            Event::QuotaHit => "quota hit",
        }
    }
}

/// The counters' values, for `status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CounterValues {
    pub refused_handshakes: u64,
    pub pairing_failures: u64,
    pub ownership_violations: u64,
    pub quota_hits: u64,
}

pub struct Counters {
    pub refused_handshakes: AtomicU64,
    pub pairing_failures: AtomicU64,
    pub ownership_violations: AtomicU64,
    pub quota_hits: AtomicU64,
    /// Per kind, when it was last logged: milliseconds after `since`, plus one (0 = never).
    logged: [AtomicU64; 4],
    since: Instant,
}

impl Default for Counters {
    fn default() -> Self {
        Counters {
            refused_handshakes: AtomicU64::new(0),
            pairing_failures: AtomicU64::new(0),
            ownership_violations: AtomicU64::new(0),
            quota_hits: AtomicU64::new(0),
            logged: Default::default(),
            since: Instant::now(),
        }
    }
}

impl Counters {
    fn counter(&self, ev: Event) -> &AtomicU64 {
        match ev {
            Event::RefusedHandshake => &self.refused_handshakes,
            Event::PairingFailure => &self.pairing_failures,
            Event::OwnershipViolation => &self.ownership_violations,
            Event::QuotaHit => &self.quota_hits,
        }
    }

    /// Counts one event and logs it, unless its kind was logged less than a minute ago; `true` if it logged.
    pub fn record(&self, ev: Event, detail: impl fmt::Display) -> bool {
        let n = self.counter(ev).fetch_add(1, Ordering::Relaxed) + 1;
        let now = self.since.elapsed().as_millis() as u64 + 1;
        let last = &self.logged[ev as usize];
        let prev = last.load(Ordering::Relaxed);
        if prev != 0 && now.saturating_sub(prev) < LOG_EVERY.as_millis() as u64 {
            return false;
        }
        // another thread logging the same kind right now wins
        if last.compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return false;
        }
        tracing::warn!("{}: {detail} ({n} so far; logged at most once a minute)", ev.label());
        true
    }

    /// Counts a refusal the session answered `pc` with.
    pub fn refusal(&self, pc: Uuid, reason: &Refusal) {
        match reason {
            Refusal::NotOwner { device } => {
                self.record(Event::OwnershipViolation, format_args!("PC {pc} claimed device {device}, which another PC owns"));
            }
            Refusal::QuotaFull | Refusal::HubFull | Refusal::TooManyDevices => {
                self.record(Event::QuotaHit, format_args!("PC {pc} was refused: {reason:?}"));
            }
            Refusal::NotSupported => {}
        }
    }

    pub fn snapshot(&self) -> CounterValues {
        CounterValues {
            refused_handshakes: self.refused_handshakes.load(Ordering::Relaxed),
            pairing_failures: self.pairing_failures.load(Ordering::Relaxed),
            ownership_violations: self.ownership_violations.load(Ordering::Relaxed),
            quota_hits: self.quota_hits.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_refusals_counted_as_quota_hits() {
        let c = Counters::default();
        let pc = Uuid::from_u128(1);
        for r in [Refusal::QuotaFull, Refusal::HubFull, Refusal::TooManyDevices] {
            c.refusal(pc, &r);
        }
        c.refusal(pc, &Refusal::NotOwner { device: Uuid::from_u128(2) });
        c.refusal(pc, &Refusal::NotSupported);
        let s = c.snapshot();
        assert_eq!((s.quota_hits, s.ownership_violations, s.refused_handshakes, s.pairing_failures), (3, 1, 0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn counters_log_once_per_minute() {
        let c = Counters::default();
        assert!(c.record(Event::QuotaHit, "a"));
        assert!(!c.record(Event::QuotaHit, "b"));
        // each kind has its own minute
        assert!(c.record(Event::PairingFailure, "c"));
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert!(!c.record(Event::QuotaHit, "d"));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(c.record(Event::QuotaHit, "e"));
        let s = c.snapshot();
        assert_eq!((s.quota_hits, s.pairing_failures, s.refused_handshakes, s.ownership_violations), (4, 1, 0, 0));
    }
}
