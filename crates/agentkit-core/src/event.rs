//! Events: der gesamte Zustand eines Laufs ergibt sich aus ihrer geordneten Folge.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{CallId, RunId};
use crate::message::{Message, StopReason, ToolOutput, Usage};

/// Aktuelle Version des Event-Formats.
pub const SCHEMA_VERSION: u16 = 1;

/// Zeitpunkt in Millisekunden seit der Unix-Epoche, immer aus dem `Clock`-Trait.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// Zeitpunkt `millis` Millisekunden später, ohne Überlauf.
    pub fn plus_millis(self, millis: u64) -> Self {
        Self(self.0.saturating_add(millis))
    }

    /// Millisekunden von `earlier` bis `self`; null, falls `earlier` später liegt.
    pub fn millis_since(self, earlier: Timestamp) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Prüfsumme über Bytes (FNV-1a, 64 Bit, als Hex). Nicht kryptografisch.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(pub String);

impl Digest {
    /// Berechnet die Prüfsumme über `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(format!("{hash:016x}"))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Ein vom Modell angeforderter Tool-Aufruf.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: CallId,
    pub name: String,
    pub input: Value,
}

/// Entscheidung der Policy. Die Ordnung ist die Strenge: `Allow < Ask < Deny`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// Wer über einen Tool-Aufruf entschieden hat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approver {
    Rule,
    Human,
}

/// Welches harte Limit einen Lauf beendet hat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Steps,
    Tokens,
    Time,
    ToolCalls,
}

/// Grund für das Ende eines Laufs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FinishReason {
    Completed,
    LimitReached { limit: LimitKind },
    Cancelled,
    Error { message: String },
}

/// Ein Event samt Position, Lauf, Zeit und Formatversion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Lückenlos pro Sitzung. Flüchtige Events tragen die Position des letzten dauerhaften.
    pub seq: u64,
    pub run_id: RunId,
    pub ts: Timestamp,
    pub schema: u16,
    pub event: Event,
}

/// Alles, was in einem Lauf passiert.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    RunStarted {
        input: Message,
        config_digest: Digest,
    },
    ModelRequested {
        step: u32,
    },
    /// Flüchtig: wird nicht gespeichert, der volle Text steht in `ModelResponded`.
    TextDelta {
        step: u32,
        text: String,
    },
    ModelResponded {
        step: u32,
        message: Message,
        usage: Usage,
        stop: StopReason,
    },
    ToolRequested {
        call: ToolCall,
    },
    ApprovalNeeded {
        call_id: CallId,
    },
    ApprovalResolved {
        call_id: CallId,
        decision: Decision,
        by: Approver,
    },
    ToolStarted {
        call_id: CallId,
    },
    ToolFinished {
        call_id: CallId,
        output: ToolOutput,
        is_error: bool,
    },
    Compacted {
        up_to_seq: u64,
        summary: Message,
    },
    RunFinished {
        reason: FinishReason,
    },
}

impl Event {
    /// Dauerhafte Events werden gespeichert, flüchtige nur an den Event-Kanal gegeben.
    pub fn is_durable(&self) -> bool {
        !matches!(self, Event::TextDelta { .. })
    }

    /// Stabiler Name der Variante, etwa für die Spalte `kind` im Store.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::RunStarted { .. } => "run_started",
            Event::ModelRequested { .. } => "model_requested",
            Event::TextDelta { .. } => "text_delta",
            Event::ModelResponded { .. } => "model_responded",
            Event::ToolRequested { .. } => "tool_requested",
            Event::ApprovalNeeded { .. } => "approval_needed",
            Event::ApprovalResolved { .. } => "approval_resolved",
            Event::ToolStarted { .. } => "tool_started",
            Event::ToolFinished { .. } => "tool_finished",
            Event::Compacted { .. } => "compacted",
            Event::RunFinished { .. } => "run_finished",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Je ein Beispiel pro Variante; auch von anderen Tests nutzbar.
    pub(crate) fn all_events() -> Vec<Event> {
        let call_id = CallId::from_parts(5, 5);
        let call = ToolCall {
            id: call_id,
            name: "read_file".into(),
            input: json!({"path": "x"}),
        };
        vec![
            Event::RunStarted {
                input: Message::user("hi"),
                config_digest: Digest::of(b"cfg"),
            },
            Event::ModelRequested { step: 1 },
            Event::TextDelta {
                step: 1,
                text: "ha".into(),
            },
            Event::ModelResponded {
                step: 1,
                message: Message::assistant("hallo"),
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 4,
                    cached_tokens: 1,
                },
                stop: StopReason::EndTurn,
            },
            Event::ToolRequested { call },
            Event::ApprovalNeeded { call_id },
            Event::ApprovalResolved {
                call_id,
                decision: Decision::Allow,
                by: Approver::Human,
            },
            Event::ToolStarted { call_id },
            Event::ToolFinished {
                call_id,
                output: ToolOutput::text("ok"),
                is_error: false,
            },
            Event::Compacted {
                up_to_seq: 9,
                summary: Message::assistant("kurz"),
            },
            Event::RunFinished {
                reason: FinishReason::LimitReached {
                    limit: LimitKind::Steps,
                },
            },
            Event::RunFinished {
                reason: FinishReason::Error {
                    message: "kaputt".into(),
                },
            },
        ]
    }

    #[test]
    fn every_event_roundtrips_in_an_envelope() {
        for (seq, event) in all_events().into_iter().enumerate() {
            let envelope = Envelope {
                seq: seq as u64,
                run_id: RunId::from_parts(1, 1),
                ts: Timestamp(1_000),
                schema: SCHEMA_VERSION,
                event,
            };
            let json = serde_json::to_string(&envelope).unwrap();
            assert_eq!(
                serde_json::from_str::<Envelope>(&json).unwrap(),
                envelope,
                "{json}"
            );
        }
    }

    #[test]
    fn only_text_delta_is_volatile() {
        for event in all_events() {
            assert_eq!(event.is_durable(), event.kind() != "text_delta");
        }
    }

    #[test]
    fn kind_matches_serde_tag() {
        for event in all_events() {
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(value["type"], event.kind());
        }
    }

    #[test]
    fn digest_is_stable_and_distinguishes() {
        assert_eq!(Digest::of(b""), Digest("cbf29ce484222325".into()));
        assert_ne!(Digest::of(b"a"), Digest::of(b"b"));
        assert_eq!(Digest::of(b"a").to_string().len(), 16);
    }

    #[test]
    fn decision_orders_by_strictness() {
        assert!(Decision::Allow < Decision::Ask && Decision::Ask < Decision::Deny);
    }

    #[test]
    fn timestamp_arithmetic_saturates() {
        assert_eq!(Timestamp(5).plus_millis(10), Timestamp(15));
        assert_eq!(Timestamp(u64::MAX).plus_millis(1), Timestamp(u64::MAX));
        assert_eq!(Timestamp(15).millis_since(Timestamp(5)), 10);
        assert_eq!(Timestamp(5).millis_since(Timestamp(15)), 0);
    }
}
