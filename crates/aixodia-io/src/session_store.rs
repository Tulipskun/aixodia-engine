//! The D1 session store, adapted to the gateway's [`SessionStore`] trait.
//!
//! The session crate deliberately does not depend on the IO crate: it owns the
//! tables and the SQL, while the gateway owns the trait the chat loop talks to.
//! This adapter is the one place the two meet.

use async_trait::async_trait;

use aixodia_session::{AgentPatch, NewSession, Session, Turn, TurnInput, UnfinishedTurn, D1};

use crate::gateway::SessionStore;

use aixodia_session::Error as SessionError;

#[async_trait]
impl SessionStore for D1 {
    async fn create(&self, params: NewSession) -> Result<Session, SessionError> {
        D1::create(self, params).await
    }

    async fn get(&self, id: &str) -> Result<Session, SessionError> {
        D1::get(self, id).await
    }

    async fn list(&self) -> Result<Vec<Session>, SessionError> {
        D1::list(self).await
    }

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        D1::delete(self, id).await
    }

    async fn save_config(&self, id: &str, saved: &Session) -> Result<(), SessionError> {
        D1::save_config(self, id, saved).await
    }

    async fn save_agent(&self, id: &str, patch: &AgentPatch) -> Result<(), SessionError> {
        D1::save_agent(self, id, patch).await
    }

    async fn turns(&self, id: &str) -> Result<Vec<Turn>, SessionError> {
        D1::turns(self, id).await
    }

    async fn history(&self, id: &str) -> Result<Vec<aixodia_provider::Message>, SessionError> {
        D1::history(self, id).await
    }

    async fn append_turns(&self, id: &str, turns: &[TurnInput]) -> Result<Vec<Turn>, SessionError> {
        D1::append_turns(self, id, turns).await
    }

    async fn append_turns_partial(
        &self,
        id: &str,
        turns: &[TurnInput],
    ) -> aixodia_session::Appended {
        D1::append_turns_partial(self, id, turns).await
    }

    async fn unfinished(&self, limit: i64) -> Result<Vec<UnfinishedTurn>, SessionError> {
        D1::unfinished(self, limit).await
    }

    async fn turns_page(
        &self,
        id: &str,
        before_seq: i64,
        limit: i64,
    ) -> Result<Vec<Turn>, SessionError> {
        D1::turns_page(self, id, before_seq, limit).await
    }
}
