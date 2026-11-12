//! Ein- und Ausgaben der Schrittfunktion.

use can_core::{
    Approver, CallId, Decision, Event, Message, ProviderError, Request, StopReason, Timestamp,
    ToolCall, ToolOutput, Usage,
};

/// Ergebnis eines Schritts: Events zum Speichern und Anwenden, Effekte zum Ausführen.
#[derive(Debug, Default, PartialEq)]
pub struct Step {
    pub events: Vec<Event>,
    pub effects: Vec<Effect>,
}

/// Was von außen in die Zustandsmaschine kommt. Jedes Effekt-Ergebnis ist ein `Input`.
#[derive(Debug)]
pub enum Input {
    Start(Message),
    /// Nach dem Falten eines gespeicherten Logs: der Zustand entscheidet, was fehlt.
    Resume,
    ModelDone {
        message: Message,
        usage: Usage,
        stop: StopReason,
    },
    ModelFailed(ProviderError),
    PolicyDecided {
        call_id: CallId,
        decision: Decision,
        by: Approver,
    },
    /// Der Aufruf ist schon vor der Policy gescheitert: unbekanntes Tool oder ungültige Argumente.
    ToolRejected {
        call_id: CallId,
        message: String,
    },
    ToolDone {
        call_id: CallId,
        output: ToolOutput,
        is_error: bool,
    },
    /// Ein fataler Tool-Fehler beendet den Lauf.
    ToolFatal {
        call_id: CallId,
        message: String,
    },
    /// Die Zusammenfassung des Verlaufs bis `up_to_seq` liegt vor.
    /// Ein Mensch gibt den Aufruf frei, aber mit geänderten Argumenten.
    ApprovedWithEdit {
        call_id: CallId,
        input: serde_json::Value,
    },
    Compacted {
        up_to_seq: u64,
        summary: Message,
    },
    /// Die Zusammenfassung ist gescheitert.
    CompactFailed(String),
    Cancel,
    Tick(Timestamp),
}

/// Was der Treiber ausführen soll.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    CallModel(Request),
    CheckPolicy(ToolCall),
    RunTool(ToolCall),
    /// Den Verlauf bis `up_to_seq` mit dieser Anfrage zusammenfassen lassen.
    Compact {
        up_to_seq: u64,
        request: Request,
    },
}
