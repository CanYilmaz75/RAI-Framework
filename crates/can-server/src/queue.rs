//! Nachrichten an Sitzungen mit laufendem Lauf: ablehnen, einreihen, einschieben, unterbrechen.

use std::collections::VecDeque;
use std::sync::Arc;

use can_core::{Message, SessionId};
use can_runtime::RunError;

use crate::meta::SessionMeta;
use crate::runs::RunManager;

impl RunManager {
    /// Nimmt eine Nachricht an die Sitzung entgegen und behandelt sie je nach `mode`, falls dort
    /// gerade ein Lauf unterwegs ist.
    pub async fn post(
        self: &Arc<Self>,
        meta: &SessionMeta,
        message: Message,
        mode: MessageMode,
    ) -> Result<Delivery, RunError> {
        let live = self.live(meta.id);
        let waiting = self
            .queued
            .lock()
            .map(|queued| queued.get(&meta.id).is_some_and(|q| !q.is_empty()))
            .unwrap_or(false);
        let enqueue = |front: bool| {
            if let Ok(mut queued) = self.queued.lock() {
                let queue = queued.entry(meta.id).or_default();
                if front {
                    queue.push_front(message.clone())
                } else {
                    queue.push_back(message.clone())
                }
            }
        };
        match (mode, live) {
            (MessageMode::Inject, Some(live)) => {
                live.controller.inject(message.clone()).await?;
                Ok(Delivery::Injected)
            }
            (MessageMode::Interrupt, Some(live)) => {
                enqueue(true);
                live.controller.cancel();
                Ok(Delivery::Interrupting)
            }
            (MessageMode::Queue, Some(_)) => {
                enqueue(false);
                Ok(Delivery::Queued)
            }
            (MessageMode::Reject, Some(_)) => Err(RunError::Interrupted),
            // Kein Lauf hier, aber schon Nachrichten in der Schlange: hinten anstellen.
            (_, None) if waiting && mode != MessageMode::Reject => {
                enqueue(false);
                Ok(Delivery::Queued)
            }
            (_, None) => {
                self.start(meta, message).await?;
                Ok(Delivery::Started)
            }
        }
    }

    /// Zahl der Nachrichten, die in der Sitzung warten.
    pub fn queued(&self, session: SessionId) -> usize {
        self.queued
            .lock()
            .map(|queued| queued.get(&session).map_or(0, VecDeque::len))
            .unwrap_or(0)
    }
}

/// Was mit einer Nachricht geschieht, wenn in der Sitzung schon ein Lauf unterwegs ist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageMode {
    /// Ablehnen (Vorgabe).
    #[default]
    Reject,
    /// Hinten anstellen; ein neuer Lauf startet, wenn der laufende endet.
    Queue,
    /// In den laufenden Lauf schieben; er berücksichtigt sie vor der nächsten Modellanfrage.
    Inject,
    /// Den laufenden Lauf abbrechen und mit dieser Nachricht neu beginnen.
    Interrupt,
}

/// Was mit der Nachricht geschehen ist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Started,
    Queued,
    Injected,
    Interrupting,
}
