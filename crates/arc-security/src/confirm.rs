//! Pending confirmations for risky actions.
//!
//! When the policy says `Confirm`, the daemon stores the *exact* tool call
//! here and hands the client a short, random, single-use id. Only
//! `take(id)` releases the stored call, which the daemon then executes
//! verbatim, so the model cannot swap the arguments between asking and
//! running. Entries expire after `permissions.confirmation_timeout_s`.
//!
//! Time is injected (`now: Instant`) so expiry is testable without sleeping.

use arc_proto::{PendingConfirmation, RiskLevel};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub id: String,
    pub tool: String,
    pub args: Value,
    pub risk: RiskLevel,
    pub explanation: String,
    /// Opaque request context (e.g. conversation / request id) so the
    /// daemon can resume the right conversation after confirmation.
    pub context: Value,
    created: Instant,
    expires: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmError {
    Unknown,
    Expired,
}

impl std::fmt::Display for ConfirmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConfirmError::Unknown => "no pending action with that id (it may already have been handled)",
            ConfirmError::Expired => "that confirmation expired; ask again if you still want it",
        })
    }
}

impl std::error::Error for ConfirmError {}

#[derive(Debug)]
pub struct Confirmations {
    ttl: Duration,
    pending: HashMap<String, Pending>,
    max_pending: usize,
}

impl Confirmations {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, pending: HashMap::new(), max_pending: 16 }
    }

    /// Store a call awaiting confirmation and return what the client shows.
    pub fn issue(
        &mut self,
        tool: &str,
        args: Value,
        risk: RiskLevel,
        explanation: &str,
        context: Value,
        now: Instant,
    ) -> PendingConfirmation {
        self.prune(now);
        if self.pending.len() >= self.max_pending {
            // Drop the oldest; unbounded growth would be a DoS vector.
            if let Some(oldest) = self.pending.values().min_by_key(|p| p.created).map(|p| p.id.clone()) {
                self.pending.remove(&oldest);
            }
        }
        let id = loop {
            let id = format!("{:08x}", rand::random::<u32>());
            if !self.pending.contains_key(&id) {
                break id;
            }
        };
        let p = Pending {
            id: id.clone(),
            tool: tool.to_string(),
            args: args.clone(),
            risk,
            explanation: explanation.to_string(),
            context,
            created: now,
            expires: now + self.ttl,
        };
        self.pending.insert(id.clone(), p);
        PendingConfirmation {
            confirmation_id: id,
            tool: tool.to_string(),
            args,
            risk,
            explanation: explanation.to_string(),
            expires_in_s: self.ttl.as_secs(),
        }
    }

    /// Consume a confirmation. Single use: a second `take` returns `Unknown`.
    pub fn take(&mut self, id: &str, now: Instant) -> Result<Pending, ConfirmError> {
        let p = self.pending.remove(id.trim()).ok_or(ConfirmError::Unknown)?;
        if now >= p.expires {
            return Err(ConfirmError::Expired);
        }
        Ok(p)
    }

    /// Discard a pending call (user said no). Returns it if it existed.
    pub fn cancel(&mut self, id: &str) -> Option<Pending> {
        self.pending.remove(id.trim())
    }

    pub fn cancel_all(&mut self) -> usize {
        let n = self.pending.len();
        self.pending.clear();
        n
    }

    /// The most recent live confirmation, used to resolve a bare spoken
    /// "yes" / "no" to the question Arc just asked.
    pub fn latest(&mut self, now: Instant) -> Option<&Pending> {
        self.prune(now);
        self.pending.values().max_by_key(|p| p.created)
    }

    pub fn list(&mut self, now: Instant) -> Vec<&Pending> {
        self.prune(now);
        let mut v: Vec<&Pending> = self.pending.values().collect();
        v.sort_by_key(|p| p.created);
        v
    }

    pub fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, p| now < p.expires);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn c() -> Confirmations {
        Confirmations::new(Duration::from_secs(90))
    }

    #[test]
    fn issue_and_take_once() {
        let mut c = c();
        let t0 = Instant::now();
        let p = c.issue(
            "files.delete",
            json!({"path": "~/x"}),
            RiskLevel::Dangerous,
            "delete x",
            json!(null),
            t0,
        );
        assert_eq!(p.expires_in_s, 90);
        assert_eq!(p.confirmation_id.len(), 8);
        let got = c.take(&p.confirmation_id, t0 + Duration::from_secs(5)).unwrap();
        assert_eq!(got.tool, "files.delete");
        assert_eq!(got.args, json!({"path": "~/x"}));
        assert_eq!(c.take(&p.confirmation_id, t0), Err(ConfirmError::Unknown));
    }

    #[test]
    fn expiry() {
        let mut c = c();
        let t0 = Instant::now();
        let p = c.issue("x", json!({}), RiskLevel::Dangerous, "", json!(null), t0);
        assert_eq!(c.take(&p.confirmation_id, t0 + Duration::from_secs(90)), Err(ConfirmError::Expired));
        let p = c.issue("x", json!({}), RiskLevel::Dangerous, "", json!(null), t0);
        assert!(c.latest(t0 + Duration::from_secs(91)).is_none());
        assert_eq!(c.take(&p.confirmation_id, t0), Err(ConfirmError::Unknown));
    }

    #[test]
    fn unknown_id_and_cancel() {
        let mut c = c();
        let t0 = Instant::now();
        assert_eq!(c.take("nope", t0), Err(ConfirmError::Unknown));
        let p = c.issue("x", json!({}), RiskLevel::Caution, "", json!(null), t0);
        assert!(c.cancel(&p.confirmation_id).is_some());
        assert_eq!(c.take(&p.confirmation_id, t0), Err(ConfirmError::Unknown));
    }

    #[test]
    fn latest_and_bound() {
        let mut c = c();
        let t0 = Instant::now();
        for i in 0..20 {
            c.issue(
                &format!("t{i}"),
                json!({}),
                RiskLevel::Caution,
                "",
                json!(null),
                t0 + Duration::from_millis(i),
            );
        }
        assert_eq!(c.list(t0).len(), 16);
        assert_eq!(c.latest(t0 + Duration::from_millis(30)).unwrap().tool, "t19");
        assert_eq!(c.cancel_all(), 16);
    }
}
