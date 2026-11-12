//! Handle auf einen laufenden Lauf: Event-Strom, Freigaben, Abbruch und Ergebnis.

use can_core::{CallId, Decision, Envelope, FinishReason, Message, StoreError, Usage};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Fehler beim Starten oder Ausführen eines Laufs.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("die Sitzung enthält einen unterbrochenen Lauf; er muss zuerst fortgesetzt werden")]
    Interrupted,
    #[error("in dieser Sitzung gibt es keinen unterbrochenen Lauf")]
    NothingToResume,
    #[error("der Lauf ist bereits beendet oder sein Empfänger wurde geschlossen")]
    Closed,
    #[error("die Eingabe wurde vom Inhaltsfilter abgelehnt: {0}")]
    Blocked(String),
    #[error("das Modell lieferte keine gültige strukturierte Antwort: {0}")]
    Structured(String),
    #[error("an dieser Position gibt es nichts zu verzweigen")]
    NothingToFork,
    #[error("die Lauf-Task ist abgestürzt: {0}")]
    Task(String),
}

/// Ergebnis eines beendeten Laufs.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub reason: FinishReason,
    /// Text der letzten Assistentennachricht.
    pub text: String,
    pub usage: Usage,
    pub steps: u32,
}

pub(crate) enum Command {
    Resolve {
        call_id: CallId,
        decision: Decision,
    },
    ApproveEdited {
        call_id: CallId,
        input: serde_json::Value,
    },
    Inject(Message),
}

/// Steuerung eines Laufs, getrennt vom Event-Strom und frei kopierbar: so kann eine Stelle
/// die Events lesen, während eine andere Freigaben erteilt oder abbricht.
#[derive(Clone)]
pub struct RunController {
    commands: mpsc::Sender<Command>,
    cancel: CancellationToken,
}

impl RunController {
    /// Beantwortet ein `ApprovalNeeded` mit `Allow` oder `Deny`.
    pub async fn resolve(&self, call_id: CallId, decision: Decision) -> Result<(), RunError> {
        self.commands
            .send(Command::Resolve { call_id, decision })
            .await
            .map_err(|_| RunError::Closed)
    }

    /// Gibt den Aufruf frei, aber mit diesen Argumenten statt denen des Modells. Passen sie
    /// nicht zum Schema des Tools, bleibt die Rückfrage offen.
    pub async fn approve_with(
        &self,
        call_id: CallId,
        input: serde_json::Value,
    ) -> Result<(), RunError> {
        self.commands
            .send(Command::ApproveEdited { call_id, input })
            .await
            .map_err(|_| RunError::Closed)
    }

    /// Schiebt eine Nachricht in den laufenden Lauf. Sie geht vor der nächsten Modellanfrage in
    /// den Verlauf; trennt dabei nie einen Tool-Aufruf von seinem Ergebnis.
    pub async fn inject(&self, message: impl Into<Message>) -> Result<(), RunError> {
        self.commands
            .send(Command::Inject(message.into()))
            .await
            .map_err(|_| RunError::Closed)
    }

    /// Bricht den Lauf ab; er endet mit `FinishReason::Cancelled`.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// Ein laufender Lauf. Wird das Handle verworfen, bricht der Lauf ab.
pub struct Run {
    events: mpsc::Receiver<Envelope>,
    commands: mpsc::Sender<Command>,
    cancel: CancellationToken,
    task: Option<JoinHandle<Result<Outcome, RunError>>>,
    outcome: Option<Outcome>,
}

impl Run {
    pub(crate) fn new(
        events: mpsc::Receiver<Envelope>,
        commands: mpsc::Sender<Command>,
        cancel: CancellationToken,
        task: JoinHandle<Result<Outcome, RunError>>,
    ) -> Self {
        Self {
            events,
            commands,
            cancel,
            task: Some(task),
            outcome: None,
        }
    }

    /// Nächstes Event; `None` nach dem Ende des Laufs. Ein Fehler des Laufs kommt genau einmal.
    pub async fn next(&mut self) -> Option<Result<Envelope, RunError>> {
        if let Some(envelope) = self.events.recv().await {
            return Some(Ok(envelope));
        }
        match self.task.take()?.await {
            Ok(Ok(outcome)) => {
                self.outcome = Some(outcome);
                None
            }
            Ok(Err(error)) => Some(Err(error)),
            Err(join) => Some(Err(RunError::Task(join.to_string()))),
        }
    }

    /// Beantwortet ein `ApprovalNeeded` mit `Allow` oder `Deny`.
    pub async fn resolve(&self, call_id: CallId, decision: Decision) -> Result<(), RunError> {
        self.commands
            .send(Command::Resolve { call_id, decision })
            .await
            .map_err(|_| RunError::Closed)
    }

    /// Gibt den Aufruf mit geänderten Argumenten frei; siehe [`RunController::approve_with`].
    pub async fn approve_with(
        &self,
        call_id: CallId,
        input: serde_json::Value,
    ) -> Result<(), RunError> {
        self.controller().approve_with(call_id, input).await
    }

    /// Schiebt eine Nachricht in den laufenden Lauf; siehe [`RunController::inject`].
    pub async fn inject(&self, message: impl Into<Message>) -> Result<(), RunError> {
        self.controller().inject(message).await
    }

    /// Steuerung, die unabhängig vom Handle weitergegeben werden kann.
    pub fn controller(&self) -> RunController {
        RunController {
            commands: self.commands.clone(),
            cancel: self.cancel.clone(),
        }
    }

    /// Bricht den Lauf ab; er endet mit `FinishReason::Cancelled`.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Token, über das sich der Lauf von außen abbrechen lässt.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Wartet auf das Ende und liefert das Ergebnis. Noch nicht gelesene Events werden verworfen.
    pub async fn outcome(&mut self) -> Result<Outcome, RunError> {
        while let Some(item) = self.next().await {
            item?;
        }
        self.outcome.clone().ok_or(RunError::Closed)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
