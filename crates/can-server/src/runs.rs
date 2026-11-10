//! Verwaltung der laufenden Läufe: starten, fortsetzen, Events verteilen, Freigaben sammeln.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use can_core::{
    CallId, Decision, Envelope, Event, FinishReason, Message, SessionId, Store, StoreError,
    ToolCall,
};
use can_runtime::{Agent, Run, RunController, RunError};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::meta::{SessionDirectory, SessionMeta, SessionStatus};
use crate::metrics::Metrics;

/// Eine offene Rückfrage.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PendingApproval {
    pub session_id: SessionId,
    pub call_id: CallId,
    pub tool: String,
    pub input: serde_json::Value,
}

/// Ein Lauf, der in diesem Prozess unterwegs ist.
pub struct Live {
    pub tenant: String,
    events: broadcast::Sender<Envelope>,
    controller: RunController,
    approvals: Mutex<HashMap<CallId, ToolCall>>,
}

impl Live {
    /// Neuer Empfänger für die Events ab jetzt.
    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.events.subscribe()
    }
}

/// Führt die Läufe dieses Prozesses.
pub struct RunManager {
    pub agents: HashMap<String, Agent>,
    pub store: Arc<dyn Store>,
    pub directory: Arc<dyn SessionDirectory>,
    pub metrics: Arc<Metrics>,
    live: Mutex<HashMap<SessionId, Arc<Live>>>,
}

fn reason_label(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Completed => "completed",
        FinishReason::LimitReached { .. } => "limit_reached",
        FinishReason::Cancelled => "cancelled",
        _ => "error",
    }
}

impl RunManager {
    pub fn new(
        agents: HashMap<String, Agent>,
        store: Arc<dyn Store>,
        directory: Arc<dyn SessionDirectory>,
    ) -> Arc<Self> {
        Arc::new(Self {
            agents,
            store,
            directory,
            metrics: Arc::new(Metrics::default()),
            live: Mutex::new(HashMap::new()),
        })
    }

    /// Der Lauf der Sitzung, falls er in diesem Prozess unterwegs ist.
    pub fn live(&self, session: SessionId) -> Option<Arc<Live>> {
        self.live.lock().ok()?.get(&session).cloned()
    }

    /// Zahl der Läufe eines Mandanten, die gerade unterwegs sind.
    pub fn running_for(&self, tenant: &str) -> usize {
        self.live
            .lock()
            .map(|live| live.values().filter(|run| run.tenant == tenant).count())
            .unwrap_or(0)
    }

    /// Zahl aller Läufe, die gerade unterwegs sind.
    pub fn running(&self) -> usize {
        self.live.lock().map(|live| live.len()).unwrap_or(0)
    }

    /// Offene Rückfragen; mit `tenant` nur die eines Mandanten.
    pub fn approvals(&self, tenant: Option<&str>) -> Vec<PendingApproval> {
        let Ok(live) = self.live.lock() else {
            return Vec::new();
        };
        let mut pending = Vec::new();
        for (session, run) in live
            .iter()
            .filter(|(_, run)| tenant.is_none_or(|tenant| run.tenant == tenant))
        {
            if let Ok(approvals) = run.approvals.lock() {
                pending.extend(approvals.values().map(|call| PendingApproval {
                    session_id: *session,
                    call_id: call.id,
                    tool: call.name.clone(),
                    input: call.input.clone(),
                }));
            }
        }
        pending.sort_by_key(|approval| approval.call_id);
        pending
    }

    /// Beantwortet eine offene Rückfrage. `false`, wenn es sie nicht (mehr) gibt.
    pub async fn resolve(&self, session: SessionId, call_id: CallId, decision: Decision) -> bool {
        let Some(live) = self.live(session) else {
            return false;
        };
        let known = live
            .approvals
            .lock()
            .map(|approvals| approvals.contains_key(&call_id))
            .unwrap_or(false);
        if known && live.controller.resolve(call_id, decision).await.is_ok() {
            self.metrics.approval_resolved();
            return true;
        }
        false
    }

    /// Bricht den Lauf der Sitzung ab. `false`, wenn hier keiner unterwegs ist.
    pub fn cancel(&self, session: SessionId) -> bool {
        self.live(session)
            .map(|live| live.controller.cancel())
            .is_some()
    }

    /// Startet einen neuen Lauf in der Sitzung.
    pub async fn start(
        self: &Arc<Self>,
        meta: &SessionMeta,
        input: Message,
    ) -> Result<(), RunError> {
        let agent = self
            .agents
            .get(&meta.agent)
            .ok_or(RunError::NothingToResume)?;
        let run = agent.run(meta.id, input).await?;
        self.adopt(meta, run, HashMap::new()).await;
        Ok(())
    }

    /// Setzt einen unterbrochenen Lauf fort, etwa nach dem Absturz eines Workers.
    pub async fn resume(self: &Arc<Self>, meta: &SessionMeta) -> Result<(), RunError> {
        let agent = self
            .agents
            .get(&meta.agent)
            .ok_or(RunError::NothingToResume)?;
        // Aufrufe aus dem Log, damit eine erneute Rückfrage zeigen kann, worum es geht.
        let mut calls = HashMap::new();
        for envelope in self.store.load(meta.id).await? {
            if let Event::ToolRequested { call } = envelope.event {
                calls.insert(call.id, call);
            }
        }
        let run = agent.resume(meta.id).await?;
        self.adopt(meta, run, calls).await;
        Ok(())
    }

    /// Versucht, alle als laufend vermerkten Sitzungen zu übernehmen, die hier nicht unterwegs
    /// sind. Hält ein anderer Worker die Sperre noch, bleibt die Sitzung bei ihm.
    pub async fn recover(self: &Arc<Self>) -> usize {
        let mut taken = 0;
        for meta in self.directory.running().await.unwrap_or_default() {
            if self.live(meta.id).is_some() {
                continue;
            }
            match self.resume(&meta).await {
                Ok(()) => taken += 1,
                Err(RunError::NothingToResume) => {
                    let _ = self
                        .directory
                        .set_status(meta.id, SessionStatus::Idle)
                        .await;
                }
                Err(error) => tracing::warn!(session = %meta.id, %error, "Übernahme nicht möglich"),
            }
        }
        taken
    }

    async fn adopt(
        self: &Arc<Self>,
        meta: &SessionMeta,
        run: Run,
        calls: HashMap<CallId, ToolCall>,
    ) {
        let (events, _) = broadcast::channel(1024);
        let live = Arc::new(Live {
            tenant: meta.tenant.clone(),
            events,
            controller: run.controller(),
            approvals: Mutex::new(HashMap::new()),
        });
        if let Ok(mut all) = self.live.lock() {
            all.insert(meta.id, live.clone());
        }
        let _ = self
            .directory
            .set_status(meta.id, SessionStatus::Running)
            .await;
        self.metrics.run_started();
        tokio::spawn(self.clone().pump(meta.clone(), run, live, calls));
    }

    /// Liest die Events des Laufs, verteilt sie und führt Buch über offene Rückfragen.
    async fn pump(
        self: Arc<Self>,
        meta: SessionMeta,
        mut run: Run,
        live: Arc<Live>,
        mut calls: HashMap<CallId, ToolCall>,
    ) {
        let mut failure = None;
        while let Some(item) = run.next().await {
            let envelope = match item {
                Ok(envelope) => envelope,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            match &envelope.event {
                Event::ToolRequested { call } => {
                    calls.insert(call.id, call.clone());
                }
                Event::ApprovalNeeded { call_id } => {
                    if let (Some(call), Ok(mut approvals)) =
                        (calls.get(call_id), live.approvals.lock())
                    {
                        approvals.insert(*call_id, call.clone());
                    }
                }
                Event::ApprovalResolved { call_id, .. } => {
                    if let Ok(mut approvals) = live.approvals.lock() {
                        approvals.remove(call_id);
                    }
                }
                _ => {}
            }
            // Ohne Zuhörer geht nichts verloren: dauerhafte Events stehen im Store.
            let _ = live.events.send(envelope);
        }
        if let Ok(mut all) = self.live.lock() {
            all.remove(&meta.id);
        }
        let status = match (failure, run.outcome().await) {
            // Ein anderer Worker führt die Sitzung: sie bleibt als laufend vermerkt.
            (Some(RunError::Store(StoreError::Locked { .. })), _) => return,
            (None, Ok(outcome)) => {
                self.metrics.run_finished(
                    reason_label(&outcome.reason),
                    &meta.tenant,
                    outcome.usage.total(),
                );
                if outcome.reason == FinishReason::Completed {
                    SessionStatus::Idle
                } else {
                    SessionStatus::Stopped
                }
            }
            (failure, _) => {
                tracing::error!(session = %meta.id, ?failure, "Lauf endete mit Fehler");
                self.metrics.run_finished("failed", &meta.tenant, 0);
                SessionStatus::Stopped
            }
        };
        let _ = self.directory.set_status(meta.id, status).await;
    }
}
