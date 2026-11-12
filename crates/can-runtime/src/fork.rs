//! Verzweigen: eine neue Sitzung, die den Verlauf einer anderen bis zu einer Position übernimmt.

use can_core::{Envelope, Event, FinishReason, Phase, RunState, SCHEMA_VERSION, SessionId};

use crate::agent::Agent;
use crate::run::RunError;

impl Agent {
    /// Kopiert das Log von `source` bis einschließlich Position `up_to_seq` in eine neue Sitzung
    /// und liefert deren Kennung. Die Quelle bleibt unverändert.
    ///
    /// Liegt der Schnitt mitten in einem Lauf, wird dieser in der neuen Sitzung als abgebrochen
    /// beendet; offene Tool-Aufrufe bekommen dabei ein Ergebnis, sodass der Verlauf gültig bleibt
    /// und die neue Sitzung sofort eine Nachricht annimmt.
    pub async fn fork(&self, source: SessionId, up_to_seq: u64) -> Result<SessionId, RunError> {
        let log = self.store().load(source).await?;
        let mut copied: Vec<Envelope> = log
            .into_iter()
            .filter(|envelope| envelope.seq <= up_to_seq)
            .collect();
        let Some(last) = copied.last() else {
            return Err(RunError::NothingToFork);
        };
        let (run_id, ts) = (last.run_id, last.ts);
        let state = RunState::fold(self.config().clone(), &copied);
        if !matches!(state.phase, Phase::Idle | Phase::Done) {
            copied.push(Envelope {
                seq: state.next_seq,
                run_id,
                ts,
                schema: SCHEMA_VERSION,
                event: Event::RunFinished {
                    reason: FinishReason::Cancelled,
                },
            });
        }
        let target = SessionId::new();
        self.store().append(target, &copied).await?;
        Ok(target)
    }
}
