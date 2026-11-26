//! The hub's pairing window (spec §4): open only while an admin runs `link`, one 8-digit code, 3 attempts in
//! total across every connection, closed by a success.

use std::net::SocketAddr;

use seek_sync::pairing::{PairInfo, PairingCode, CODE_ATTEMPTS, HUB_CODE_DIGITS};
use seek_sync::peers::{key_from_b64, key_to_b64};
use seek_sync::Role;
use serde::{Deserialize, Serialize};

use crate::config::{HubConfig, DEFAULT_PORT};
use crate::store::{PcRecord, Store};

/// The open window: its code and what is left of it.
pub struct PairWindow {
    code: PairingCode,
}

impl PairWindow {
    pub fn open(now_ms: i64) -> PairWindow {
        PairWindow { code: PairingCode::with_digits(now_ms, HUB_CODE_DIGITS) }
    }

    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    pub fn usable(&self, now_ms: i64) -> bool {
        self.code.usable(now_ms)
    }

    /// Uses up one attempt before SPAKE2 runs (so parallel connections cannot get more than three guesses);
    /// the code to run it with, or `None` when the window is spent or expired.
    pub fn take_attempt(&mut self, now_ms: i64) -> Option<String> {
        (self.code.usable(now_ms) && self.code.consume_attempt()).then(|| self.code.code.clone())
    }
}

/// How a pairing window ended, for `seek-hub link`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PairOutcome {
    Linked { name: String },
    Expired,
    TooManyFailures,
    /// A PC paired but could not be linked (bad info, or the store could not save it).
    Failed { reason: String },
}

/// The pairing window as `status` shows it. `window` numbers the windows opened since the hub started (0 =
/// none yet); `outcome` is how that window ended, `None` while it is open or an attempt is still running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingState {
    pub window: u64,
    pub open: bool,
    pub outcome: Option<PairOutcome>,
}

/// The hub's pairing state: the open window, if any, and how the last one ended.
#[derive(Default)]
pub struct Pairing {
    window: Option<PairWindow>,
    seq: u64,
    failures: u8,
    outcome: Option<PairOutcome>,
}

impl Pairing {
    /// Opens a new window (replacing any open one) and returns its code.
    pub fn open(&mut self, now_ms: i64) -> PairingCode {
        let w = PairWindow::open(now_ms);
        let code = w.code().clone();
        *self = Pairing { window: Some(w), seq: self.seq + 1, failures: 0, outcome: None };
        code
    }

    pub fn is_open(&self, now_ms: i64) -> bool {
        self.window.as_ref().is_some_and(|w| w.usable(now_ms))
    }

    /// An attempt at the open window: its code and the window's number, for [`Pairing::finish`].
    pub fn take_attempt(&mut self, now_ms: i64) -> Option<(String, u64)> {
        let code = self.window.as_mut()?.take_attempt(now_ms)?;
        Some((code, self.seq))
    }

    /// An attempt at window `seq` failed; the third failure closes it. (Attempts at an older window, replaced
    /// by an admin meanwhile, change nothing here and below.)
    pub fn failed(&mut self, seq: u64) {
        if seq != self.seq || self.outcome.is_some() {
            return;
        }
        self.failures += 1;
        if self.failures >= CODE_ATTEMPTS {
            self.window = None;
            self.outcome = Some(PairOutcome::TooManyFailures);
        }
    }

    /// A PC paired through window `seq`: the window closes now, [`Pairing::linked`] follows.
    pub fn paired(&mut self, seq: u64) {
        if seq == self.seq {
            self.window = None;
        }
    }

    /// How linking the PC that paired through window `seq` ended.
    pub fn linked(&mut self, seq: u64, outcome: PairOutcome) {
        if seq == self.seq && self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
    }

    /// The state at `now_ms`. A window counts as expired `grace_ms` after its end, so an attempt that began
    /// just before the end can still finish (and link) first.
    pub fn state(&mut self, now_ms: i64, grace_ms: i64) -> PairingState {
        if self.outcome.is_none() && self.window.as_ref().is_some_and(|w| now_ms >= w.code.expires_ms + grace_ms) {
            self.window = None;
            self.outcome = Some(PairOutcome::Expired);
        }
        PairingState { window: self.seq, open: self.is_open(now_ms), outcome: self.outcome.clone() }
    }
}

/// Where PCs can reach the hub, as told in the pairing reply: the specific (non-loopback) listen addresses,
/// then the public one.
pub fn hub_addrs(cfg: &HubConfig, bound: &[SocketAddr]) -> Vec<String> {
    let mut addrs: Vec<String> = bound
        .iter()
        .filter(|a| !a.ip().is_loopback() && !a.ip().is_unspecified())
        .map(|a| a.to_string())
        .collect();
    if let Some(p) = &cfg.public {
        addrs.push(p.to_string());
    }
    addrs
}

/// What the hub tells a PC it pairs with.
pub fn hub_info(store: &Store, addrs: Vec<String>) -> PairInfo {
    PairInfo {
        host_id: store.hub_id(),
        name: store.name(),
        public_key: key_to_b64(&store.noise_keys().public),
        user_sid: String::new(),
        members: Vec::new(),
        port: DEFAULT_PORT,
        role: Role::Hub,
        addrs,
        user_name: String::new(),
    }
}

/// Longest name kept for a PC or its user.
const MAX_NAME_CHARS: usize = 64;
const MAX_SID_BYTES: usize = 512;

/// `s` without control characters (no log or terminal injection), cut to [`MAX_NAME_CHARS`].
fn clean_name(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect::<String>().trim().to_string()
}

/// The record of a PC that just paired; `Err` names why its info cannot be linked.
pub fn pc_record(guest: PairInfo, now_ms: i64) -> Result<PcRecord, String> {
    let name = clean_name(&guest.name);
    if guest.role != Role::Pc {
        return Err(format!("{name} is not a PC"));
    }
    if key_from_b64(&guest.public_key).is_none() {
        return Err(format!("{name} sent a bad key"));
    }
    if guest.user_sid.len() > MAX_SID_BYTES || guest.user_sid.chars().any(|c| c.is_control()) {
        return Err(format!("{name} sent a bad user id"));
    }
    Ok(PcRecord {
        host_id: guest.host_id,
        name,
        public_key: guest.public_key,
        user_sid: guest.user_sid,
        user_name: clean_name(&guest.user_name),
        profiles: Vec::new(),
        last_seen_ms: now_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use seek_sync::transport::HubAddr;

    #[test]
    fn window_takes_three_attempts_then_closes() {
        let mut w = PairWindow::open(1_000);
        let code = w.code().code.clone();
        assert_eq!(code.len(), 8);
        for _ in 0..3 {
            assert!(w.usable(1_000));
            assert_eq!(w.take_attempt(1_000).as_deref(), Some(code.as_str()));
        }
        assert!(!w.usable(1_000));
        assert_eq!(w.take_attempt(1_000), None);
    }

    #[test]
    fn window_expires_after_2_min() {
        let mut w = PairWindow::open(1_000);
        assert!(w.take_attempt(1_000 + 119_999).is_some());
        assert!(!w.usable(1_000 + 120_000));
        assert_eq!(w.take_attempt(1_000 + 120_000), None);
    }

    #[test]
    fn pairing_outcomes() {
        let mut p = Pairing::default();
        assert_eq!(p.state(0, 5_000), PairingState { window: 0, open: false, outcome: None });

        // expired: only once the grace for a late attempt is over
        p.open(1_000);
        assert!(p.state(1_000, 5_000).open);
        let s = p.state(121_000, 5_000);
        assert_eq!((s.open, s.outcome), (false, None));
        assert_eq!(p.state(126_000, 5_000).outcome, Some(PairOutcome::Expired));

        // three failures
        p.open(200_000);
        for _ in 0..3 {
            let (_, seq) = p.take_attempt(200_000).unwrap();
            assert_eq!(seq, 2);
            p.failed(seq);
        }
        assert_eq!(p.state(200_000, 5_000).outcome, Some(PairOutcome::TooManyFailures));

        // linked; a late failure of the same window and attempts at an old window change nothing
        p.open(300_000);
        let (_, a) = p.take_attempt(300_000).unwrap();
        let (_, b) = p.take_attempt(300_000).unwrap();
        p.paired(a);
        assert!(!p.state(300_000, 5_000).open);
        p.linked(a, PairOutcome::Linked { name: "Laptop".into() });
        p.failed(b);
        p.failed(2);
        let s = p.state(900_000, 5_000);
        assert_eq!((s.window, s.outcome), (3, Some(PairOutcome::Linked { name: "Laptop".into() })));

        // a window replaced while an attempt ran: the old attempt's success does not close the new one
        let (_, old) = (p.open(1_000_000), 4);
        p.open(1_000_000);
        p.paired(old);
        p.linked(old, PairOutcome::Linked { name: "Old".into() });
        assert_eq!(p.state(1_000_000, 5_000), PairingState { window: 5, open: true, outcome: None });
    }

    #[test]
    fn addrs_skip_loopback_and_wildcard() {
        let cfg = HubConfig {
            name: "H".into(),
            listen: vec![],
            public: Some(HubAddr::parse("tls://seek-hub.tail1234.ts.net").unwrap()),
            data_dir: "/d".into(),
            keys_dir: "/k".into(),
            pc_quota_bytes: 1,
            pc_max_devices: 1,
        };
        let bound: Vec<SocketAddr> = ["127.0.0.1:47321", "0.0.0.0:47321", "192.168.1.5:47321"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        let public = cfg.public.as_ref().unwrap().to_string();
        assert_eq!(hub_addrs(&cfg, &bound), vec!["192.168.1.5:47321".to_string(), public]);
    }

    #[test]
    fn guest_must_be_a_pc_with_a_key() {
        let good = PairInfo {
            host_id: uuid::Uuid::from_u128(5),
            name: "Laptop".into(),
            public_key: key_to_b64(&[3; 32]),
            user_sid: "S-1-5".into(),
            members: vec![],
            port: 47321,
            role: Role::Pc,
            addrs: vec![],
            user_name: "me".into(),
        };
        let rec = pc_record(good.clone(), 9).unwrap();
        assert_eq!((rec.host_id, rec.last_seen_ms, rec.user_name.as_str()), (good.host_id, 9, "me"));
        assert!(pc_record(PairInfo { role: Role::Hub, ..good.clone() }, 9).is_err());
        assert!(pc_record(PairInfo { public_key: "nope".into(), ..good.clone() }, 9).is_err());
        assert!(pc_record(PairInfo { user_sid: "S".repeat(600), ..good.clone() }, 9).is_err());
        let messy = PairInfo { name: "bad[2Jname
".into(), user_name: "u".repeat(200), ..good };
        let rec = pc_record(messy, 9).unwrap();
        assert_eq!(rec.name, "bad[2Jname");
        assert_eq!(rec.user_name.chars().count(), 64);
    }
}
