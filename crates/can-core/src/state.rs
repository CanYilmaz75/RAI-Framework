//! Zustand eines Laufs. Er entsteht ausschließlich durch Falten der Event-Folge.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::event::{Digest, Envelope, Event, FinishReason, Timestamp, ToolCall};
use crate::ids::{CallId, RunId};
use crate::message::{ContentBlock, Message, Role, ToolOutput, Usage};
use crate::tool::ToolSpec;

/// Harte Limits eines Laufs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub max_steps: u32,
    pub max_tokens: u64,
    pub timeout: Duration,
    pub max_tool_calls_per_step: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 50,
            max_tokens: 200_000,
            timeout: Duration::from_secs(600),
            max_tool_calls_per_step: 16,
        }
    }
}

/// Unveränderliche Konfiguration, mit der ein Zustand gefaltet wird.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub model: String,
    pub system: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub limits: Limits,
    pub max_output: Option<u32>,
    pub extra: Map<String, Value>,
    /// Kontextfenster des Modells in Token; null heißt unbekannt, dann wird nie kompaktiert.
    #[serde(default)]
    pub context_window: u32,
    /// Token, die für die Antwort des Modells frei bleiben müssen.
    #[serde(default)]
    pub reserve_output: u32,
}

impl RunConfig {
    /// Prüfsumme der Konfiguration; steht in `RunStarted`.
    pub fn digest(&self) -> Digest {
        Digest::of(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }

    /// Spec eines Tools nach Name.
    pub fn tool(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.iter().find(|spec| spec.name == name)
    }
}

/// Zustände der Zustandsmaschine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    AwaitModel,
    AwaitPolicy,
    RunTools,
    Done,
}

/// Stand eines einzelnen Tool-Aufrufs der aktuellen Modellantwort.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CallStatus {
    Requested,
    AwaitingApproval,
    Allowed,
    Denied,
    Started,
    Finished { output: ToolOutput, is_error: bool },
}

/// Ein Tool-Aufruf und sein Stand.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingCall {
    pub call: ToolCall,
    pub status: CallStatus,
}

impl PendingCall {
    /// Wartet der Aufruf noch auf eine Policy-Entscheidung?
    pub fn is_unresolved(&self) -> bool {
        matches!(
            self.status,
            CallStatus::Requested | CallStatus::AwaitingApproval
        )
    }
}

/// Stand der Kontextverdichtung: alles bis `up_to_seq` ist durch `summary` ersetzt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    pub up_to_seq: u64,
    pub summary: Message,
}

/// Der gesamte Zustand einer Sitzung samt laufendem Lauf.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    pub config: RunConfig,
    pub phase: Phase,
    pub run_id: Option<RunId>,
    /// Gesprächsverlauf der Sitzung über alle Läufe.
    pub messages: Vec<Message>,
    /// Position des Events, das die jeweilige Nachricht erzeugt hat; parallel zu `messages`.
    pub message_seqs: Vec<u64>,
    /// Position der Eingabe des aktuellen Laufs, der ursprünglichen Aufgabe.
    pub task_seq: Option<u64>,
    /// Letzte Kontextverdichtung; die Originale bleiben in `messages` und im Store.
    pub compaction: Option<Compaction>,
    /// Zahl der Modellanfragen im aktuellen Lauf.
    pub step: u32,
    /// Tool-Aufrufe der letzten Modellantwort, in der Reihenfolge des Modells.
    pub pending: Vec<PendingCall>,
    /// Token-Verbrauch des aktuellen Laufs.
    pub usage: Usage,
    pub started_at: Option<Timestamp>,
    /// Position, die das nächste dauerhafte Event bekommt.
    pub next_seq: u64,
    pub finish: Option<FinishReason>,
}

impl RunState {
    /// Leerer Zustand vor dem ersten Event.
    pub fn new(config: RunConfig) -> Self {
        Self {
            config,
            phase: Phase::Idle,
            run_id: None,
            messages: Vec::new(),
            message_seqs: Vec::new(),
            task_seq: None,
            compaction: None,
            step: 0,
            pending: Vec::new(),
            usage: Usage::default(),
            started_at: None,
            next_seq: 0,
            finish: None,
        }
    }

    /// Faltet eine gespeicherte Event-Folge zu einem Zustand.
    pub fn fold(config: RunConfig, log: &[Envelope]) -> Self {
        let mut state = Self::new(config);
        for envelope in log {
            state.apply_envelope(envelope);
        }
        state
    }

    /// Wendet ein dauerhaftes Event samt Position, Lauf und Zeit an.
    pub fn apply_envelope(&mut self, envelope: &Envelope) {
        self.next_seq = envelope.seq;
        self.apply(&envelope.event);
        if matches!(envelope.event, Event::RunStarted { .. }) {
            self.run_id = Some(envelope.run_id);
            self.started_at = Some(envelope.ts);
        }
    }

    /// Die einzige Stelle, die Zustand verändert.
    pub fn apply(&mut self, event: &Event) {
        // Ein dauerhaftes Event belegt die Position `next_seq` und rückt sie um eins weiter.
        let seq = self.next_seq;
        if event.is_durable() {
            self.next_seq = seq.saturating_add(1);
        }
        match event {
            Event::RunStarted { input, .. } => {
                self.push_message(input.clone(), seq);
                self.task_seq = Some(seq);
                self.phase = Phase::AwaitModel;
                self.step = 0;
                self.pending.clear();
                self.usage = Usage::default();
                self.finish = None;
                self.run_id = None;
                self.started_at = None;
            }
            Event::ModelRequested { step } => {
                self.step = *step;
                self.phase = Phase::AwaitModel;
            }
            Event::ModelResponded { message, usage, .. } => {
                self.push_message(message.clone(), seq);
                self.usage.add(*usage);
            }
            Event::ToolRequested { call } => {
                self.pending.push(PendingCall {
                    call: call.clone(),
                    status: CallStatus::Requested,
                });
                self.phase = Phase::AwaitPolicy;
            }
            Event::ApprovalNeeded { call_id } => {
                self.set_status(*call_id, CallStatus::AwaitingApproval)
            }
            Event::ApprovalResolved {
                call_id, decision, ..
            } => {
                let status = match decision {
                    crate::event::Decision::Allow => CallStatus::Allowed,
                    crate::event::Decision::Deny => CallStatus::Denied,
                    crate::event::Decision::Ask => CallStatus::AwaitingApproval,
                };
                self.set_status(*call_id, status);
                self.settle(seq);
            }
            Event::ToolStarted { call_id } => {
                self.set_status(*call_id, CallStatus::Started);
                self.phase = Phase::RunTools;
            }
            Event::ToolFinished {
                call_id,
                output,
                is_error,
            } => {
                let status = CallStatus::Finished {
                    output: output.clone(),
                    is_error: *is_error,
                };
                self.set_status(*call_id, status);
                self.settle(seq);
            }
            Event::RunFinished { reason } => {
                self.close_pending(seq);
                self.phase = Phase::Done;
                self.finish = Some(reason.clone());
            }
            Event::Compacted { up_to_seq, summary } => {
                self.compaction = Some(Compaction {
                    up_to_seq: *up_to_seq,
                    summary: summary.clone(),
                });
            }
            Event::TextDelta { .. } => {}
        }
    }

    /// Aufruf nach Kennung.
    pub fn pending_call(&self, id: CallId) -> Option<&PendingCall> {
        self.pending.iter().find(|pending| pending.call.id == id)
    }

    /// Der Zeitpunkt, ab dem das Zeitlimit des Laufs überschritten ist.
    pub fn deadline(&self) -> Option<Timestamp> {
        let millis = u64::try_from(self.config.limits.timeout.as_millis()).unwrap_or(u64::MAX);
        self.started_at.map(|start| start.plus_millis(millis))
    }

    /// Text der letzten Assistentennachricht.
    pub fn last_assistant_text(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(Message::text)
            .unwrap_or_default()
    }

    fn push_message(&mut self, message: Message, seq: u64) {
        self.messages.push(message);
        self.message_seqs.push(seq);
    }

    fn set_status(&mut self, id: CallId, status: CallStatus) {
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.call.id == id)
        {
            pending.status = status;
        }
    }

    /// Nach jeder Entscheidung und jedem Ergebnis: Sind alle Aufrufe entschieden, geht es in
    /// `RunTools`; sind alle fertig, wandern die Ergebnisse in Aufrufreihenfolge in den Verlauf.
    fn settle(&mut self, seq: u64) {
        if self.pending.is_empty() || self.pending.iter().any(PendingCall::is_unresolved) {
            return;
        }
        self.phase = Phase::RunTools;
        if self
            .pending
            .iter()
            .all(|pending| matches!(pending.status, CallStatus::Finished { .. }))
        {
            self.close_pending(seq);
        }
    }

    /// Schreibt für jeden offenen Aufruf ein Ergebnis in den Verlauf, damit dieser gültig bleibt.
    fn close_pending(&mut self, seq: u64) {
        if self.pending.is_empty() {
            return;
        }
        let content = self
            .pending
            .drain(..)
            .map(|pending| match pending.status {
                CallStatus::Finished { output, is_error } => ContentBlock::ToolResult {
                    call_id: pending.call.id,
                    output,
                    is_error,
                },
                _ => ContentBlock::ToolResult {
                    call_id: pending.call.id,
                    output: ToolOutput::text(
                        "Der Lauf endete, bevor dieses Tool ein Ergebnis lieferte.",
                    ),
                    is_error: true,
                },
            })
            .collect();
        self.push_message(
            Message {
                role: Role::Tool,
                content,
            },
            seq,
        );
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
