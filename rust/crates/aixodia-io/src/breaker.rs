//! The provider circuit breaker.
//!
//! Port of the Go `io/gateway/breaker.go`. A provider that fails repeatedly is
//! paused for a while so every chat does not spend its retry budget on it first.
//! Only upstream-side failures count: a rejected key or a bad request says
//! nothing about whether the provider is up.

use std::time::Instant;

use crate::gateway::Gateway;

pub const BREAKER_THRESHOLD: usize = 3;
pub const BREAKER_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(45);

#[derive(Debug, Clone, Default)]
pub struct Breaker {
    pub failures: usize,
    pub open_until: Option<Instant>,
}

/// A turn that was cancelled by the client, which says nothing about the
/// provider. The Go code recognises `context.Canceled` here.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "context canceled")
    }
}

impl std::error::Error for Cancelled {}

impl Gateway {
    pub(crate) fn circuit_open(&self, name: &str, now: Instant) -> bool {
        let breakers = self.breakers.lock().unwrap_or_else(|e| e.into_inner());
        breakers
            .get(name)
            .and_then(|breaker| breaker.open_until)
            .is_some_and(|open_until| now < open_until)
    }

    pub(crate) fn record_outcome(&self, name: &str, err: Option<&anyhow::Error>, now: Instant) {
        let mut breakers = self.breakers.lock().unwrap_or_else(|e| e.into_inner());
        let breaker = breakers.entry(name.to_string()).or_default();
        let Some(err) = err else {
            breaker.failures = 0;
            breaker.open_until = None;
            return;
        };
        if !counts_against_provider(err) {
            return;
        }
        breaker.failures += 1;
        if breaker.failures >= BREAKER_THRESHOLD {
            breaker.open_until = Some(now + BREAKER_COOLDOWN);
            breaker.failures = 0;
        }
    }
}

pub fn counts_against_provider(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<Cancelled>().is_some() {
        return false;
    }
    if let Some(api) = err.downcast_ref::<aixodia_provider::APIError>() {
        // 429 and 402 say "slow down", not "down": the provider answers again
        // once the free-tier window passes, and a pause would replace its own
        // message with a misleading one.
        return api.status_code == 408 || api.status_code >= 500;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{Gateway, SessionStore};
    use aixodia_provider::{APIError, Registry};
    use std::collections::HashMap;
    use std::sync::Arc;

    struct NoStore;

    #[async_trait::async_trait]
    impl SessionStore for NoStore {
        async fn create(
            &self,
            _params: aixodia_session::NewSession,
        ) -> Result<aixodia_session::Session, aixodia_session::Error> {
            Err(aixodia_session::Error::NotFound("none".into()))
        }
        async fn get(&self, id: &str) -> Result<aixodia_session::Session, aixodia_session::Error> {
            Err(aixodia_session::Error::NotFound(id.into()))
        }
        async fn list(&self) -> Result<Vec<aixodia_session::Session>, aixodia_session::Error> {
            Ok(Vec::new())
        }
        async fn delete(&self, _id: &str) -> Result<(), aixodia_session::Error> {
            Ok(())
        }
        async fn save_config(
            &self,
            _id: &str,
            _saved: &aixodia_session::Session,
        ) -> Result<(), aixodia_session::Error> {
            Ok(())
        }
        async fn save_agent(
            &self,
            _id: &str,
            _patch: &aixodia_session::AgentPatch,
        ) -> Result<(), aixodia_session::Error> {
            Ok(())
        }
        async fn turns(
            &self,
            _id: &str,
        ) -> Result<Vec<aixodia_session::Turn>, aixodia_session::Error> {
            Ok(Vec::new())
        }
        async fn history(
            &self,
            _id: &str,
        ) -> Result<Vec<aixodia_provider::Message>, aixodia_session::Error> {
            Ok(Vec::new())
        }
        async fn append_turns(
            &self,
            _id: &str,
            _turns: &[aixodia_session::TurnInput],
        ) -> Result<Vec<aixodia_session::Turn>, aixodia_session::Error> {
            Ok(Vec::new())
        }
        async fn unfinished(
            &self,
            _limit: i64,
        ) -> Result<Vec<aixodia_session::UnfinishedTurn>, aixodia_session::Error> {
            Ok(Vec::new())
        }
    }

    fn gateway() -> Gateway {
        Gateway::new(Arc::new(NoStore), Arc::new(Registry::new()), HashMap::new())
    }

    #[test]
    fn three_upstream_failures_open_the_breaker() {
        let g = gateway();
        let now = Instant::now();
        assert!(!g.circuit_open("P", now));
        for _ in 0..BREAKER_THRESHOLD {
            g.record_outcome("P", Some(&APIError::new(503, "down").into_anyhow()), now);
        }
        assert!(g.circuit_open("P", now));
        assert!(!g.circuit_open(
            "P",
            now + BREAKER_COOLDOWN + std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn a_success_clears_the_breaker() {
        let g = gateway();
        let now = Instant::now();
        g.record_outcome("P", Some(&APIError::new(503, "down").into_anyhow()), now);
        g.record_outcome("P", None, now);
        g.record_outcome("P", Some(&APIError::new(503, "down").into_anyhow()), now);
        g.record_outcome("P", Some(&APIError::new(503, "down").into_anyhow()), now);
        // Only two failures in a row, so it is still closed.
        assert!(!g.circuit_open("P", now));
    }

    #[test]
    fn a_rejected_key_does_not_count_against_the_provider() {
        let g = gateway();
        let now = Instant::now();
        for _ in 0..BREAKER_THRESHOLD {
            g.record_outcome("P", Some(&APIError::new(401, "bad key").into_anyhow()), now);
        }
        assert!(!g.circuit_open("P", now));
        assert!(!counts_against_provider(&Cancelled.into()));
    }

    #[test]
    fn a_rate_limit_does_not_pause_the_provider() {
        let g = gateway();
        let now = Instant::now();
        for _ in 0..BREAKER_THRESHOLD {
            g.record_outcome(
                "P",
                Some(&APIError::new(429, "slow down").into_anyhow()),
                now,
            );
        }
        assert!(!g.circuit_open("P", now));
        assert!(!counts_against_provider(
            &APIError::new(429, "slow down").into_anyhow()
        ));
        assert!(!counts_against_provider(
            &APIError::new(402, "free tier quota").into_anyhow()
        ));
    }
}
