//! Läufe in einem Namensraum, etwa pro Mandant: Tools sehen ihn als `ToolCtx::namespace`.

use can_core::{Message, SessionId};

use crate::agent::Agent;
use crate::run::{Run, RunError};

impl Agent {
    /// Wie [`Agent::run`], im angegebenen Namensraum.
    pub async fn run_in(
        &self,
        namespace: impl Into<String>,
        session: SessionId,
        input: impl Into<Message>,
    ) -> Result<Run, RunError> {
        self.start(session, Some(input.into()), true, namespace.into())
            .await
    }

    /// Wie [`Agent::resume`], im angegebenen Namensraum.
    pub async fn resume_in(
        &self,
        namespace: impl Into<String>,
        session: SessionId,
    ) -> Result<Run, RunError> {
        self.start(session, None, true, namespace.into()).await
    }

    /// Wie [`Agent::run_unattended`], im angegebenen Namensraum.
    pub async fn run_unattended_in(
        &self,
        namespace: impl Into<String>,
        session: SessionId,
        input: impl Into<Message>,
    ) -> Result<Run, RunError> {
        self.start(session, Some(input.into()), false, namespace.into())
            .await
    }
}
