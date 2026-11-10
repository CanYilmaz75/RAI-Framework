//! Die reine Schrittfunktion: entscheidet, macht aber selbst nie Ein- oder Ausgabe.

use can_core::{
    Approver, CallId, CallStatus, Decision, Event, FinishReason, LimitKind, Message, Phase,
    ProviderError, Request, RunState, StopReason, Timestamp, ToolCall, ToolOutput, Usage,
};

use crate::context::build_request;

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
    Cancel,
    Tick(Timestamp),
}

/// Was der Treiber ausführen soll.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    CallModel(Request),
    CheckPolicy(ToolCall),
    RunTool(ToolCall),
}

/// Entscheidet anhand von Zustand und Eingabe. Deterministisch und total: unpassende Eingaben
/// ergeben einen leeren Schritt.
pub fn step(state: &RunState, input: Input) -> Step {
    let mut em = Emitter {
        state: state.clone(),
        out: Step::default(),
    };
    let running = matches!(
        state.phase,
        Phase::AwaitModel | Phase::AwaitPolicy | Phase::RunTools
    );
    match (state.phase, input) {
        (Phase::Idle | Phase::Done, Input::Start(message)) => {
            let config_digest = state.config.digest();
            em.emit(Event::RunStarted {
                input: message,
                config_digest,
            });
            em.request_model();
        }
        (
            Phase::AwaitModel,
            Input::ModelDone {
                message,
                usage,
                stop,
            },
        ) => {
            em.model_done(message, usage, stop);
        }
        (Phase::AwaitModel, Input::ModelFailed(error)) => {
            em.finish(FinishReason::Error {
                message: error.to_string(),
            });
        }
        (
            Phase::AwaitPolicy,
            Input::PolicyDecided {
                call_id,
                decision,
                by,
            },
        ) if em.unresolved(call_id) => match decision {
            Decision::Ask => em.emit(Event::ApprovalNeeded { call_id }),
            Decision::Allow => {
                em.emit(Event::ApprovalResolved {
                    call_id,
                    decision,
                    by,
                });
                em.advance();
            }
            Decision::Deny => {
                em.emit(Event::ApprovalResolved {
                    call_id,
                    decision,
                    by,
                });
                em.tool_error(call_id, "Der Aufruf wurde abgelehnt und nicht ausgeführt.");
                em.advance();
            }
        },
        (Phase::AwaitPolicy, Input::ToolRejected { call_id, message })
            if em.unresolved(call_id) =>
        {
            em.tool_error(call_id, &message);
            em.advance();
        }
        (
            Phase::RunTools,
            Input::ToolDone {
                call_id,
                output,
                is_error,
            },
        ) if em.started(call_id) => {
            em.emit(Event::ToolFinished {
                call_id,
                output,
                is_error,
            });
            em.advance();
        }
        (Phase::RunTools, Input::ToolFatal { call_id, message }) if em.started(call_id) => {
            em.tool_error(call_id, &message);
            em.finish(FinishReason::Error { message });
        }
        (_, Input::Cancel) if running => em.finish(FinishReason::Cancelled),
        (_, Input::Tick(now))
            if running && state.deadline().is_some_and(|deadline| now >= deadline) =>
        {
            em.finish(FinishReason::LimitReached {
                limit: LimitKind::Time,
            });
        }
        (_, Input::Resume) if running => em.resume(),
        _ => {}
    }
    em.out
}

/// Sammelt Events und Effekte und führt dabei eine Arbeitskopie des Zustands mit.
struct Emitter {
    state: RunState,
    out: Step,
}

impl Emitter {
    fn emit(&mut self, event: Event) {
        self.state.apply(&event);
        self.out.events.push(event);
    }

    fn finish(&mut self, reason: FinishReason) {
        self.emit(Event::RunFinished { reason });
    }

    fn tool_error(&mut self, call_id: CallId, message: &str) {
        self.emit(Event::ToolFinished {
            call_id,
            output: ToolOutput::text(message),
            is_error: true,
        });
    }

    fn unresolved(&self, id: CallId) -> bool {
        self.state
            .pending_call(id)
            .is_some_and(|pending| pending.is_unresolved())
    }

    fn started(&self, id: CallId) -> bool {
        self.state
            .pending_call(id)
            .is_some_and(|pending| pending.status == CallStatus::Started)
    }

    /// Nächste Modellanfrage, oder Ende des Laufs am Schrittlimit.
    fn request_model(&mut self) {
        let next = self.state.step.saturating_add(1);
        if next > self.state.config.limits.max_steps {
            return self.finish(FinishReason::LimitReached {
                limit: LimitKind::Steps,
            });
        }
        self.emit(Event::ModelRequested { step: next });
        self.out
            .effects
            .push(Effect::CallModel(build_request(&self.state)));
    }

    fn model_done(&mut self, message: Message, usage: Usage, stop: StopReason) {
        let calls = message.tool_uses();
        let limits = self.state.config.limits.clone();
        self.emit(Event::ModelResponded {
            step: self.state.step,
            message,
            usage,
            stop,
        });
        if self.state.usage.total() > limits.max_tokens {
            self.finish(FinishReason::LimitReached {
                limit: LimitKind::Tokens,
            });
        } else if calls.is_empty() {
            self.finish(FinishReason::Completed);
        } else if calls.len() > limits.max_tool_calls_per_step as usize {
            self.finish(FinishReason::LimitReached {
                limit: LimitKind::ToolCalls,
            });
        } else {
            for call in calls {
                self.emit(Event::ToolRequested { call: call.clone() });
                self.out.effects.push(Effect::CheckPolicy(call));
            }
        }
    }

    /// Sobald alle Aufrufe entschieden sind: Tools einzeln in Modellreihenfolge starten, danach
    /// das Modell erneut fragen. Solange ein Tool läuft oder Entscheidungen fehlen, passiert nichts.
    fn advance(&mut self) {
        if self.state.phase != Phase::RunTools {
            return;
        }
        if self.state.pending.is_empty() {
            return self.request_model();
        }
        if self
            .state
            .pending
            .iter()
            .any(|pending| pending.status == CallStatus::Started)
        {
            return;
        }
        let next = self
            .state
            .pending
            .iter()
            .find(|pending| pending.status == CallStatus::Allowed);
        if let Some(call) = next.map(|pending| pending.call.clone()) {
            self.emit(Event::ToolStarted { call_id: call.id });
            self.out.effects.push(Effect::RunTool(call));
        }
    }

    /// Wiederaufnahme: was im Log begonnen, aber nicht beendet wurde, wird nachgeholt.
    fn resume(&mut self) {
        match self.state.phase {
            Phase::AwaitModel => self
                .out
                .effects
                .push(Effect::CallModel(build_request(&self.state))),
            Phase::AwaitPolicy => {
                let open = self
                    .state
                    .pending
                    .iter()
                    .filter(|pending| pending.is_unresolved());
                let effects: Vec<Effect> =
                    open.map(|p| Effect::CheckPolicy(p.call.clone())).collect();
                self.out.effects.extend(effects);
            }
            Phase::RunTools => {
                let started = self
                    .state
                    .pending
                    .iter()
                    .find(|p| p.status == CallStatus::Started);
                match started.map(|pending| pending.call.clone()) {
                    Some(call)
                        if self
                            .state
                            .config
                            .tool(&call.name)
                            .is_some_and(|s| s.idempotent) =>
                    {
                        self.out.effects.push(Effect::RunTool(call));
                    }
                    Some(call) => {
                        // Nicht idempotent: nie unbemerkt doppelt ausführen.
                        self.tool_error(call.id, "Unterbrochen, Ausgang unbekannt.");
                        self.advance();
                    }
                    None => self.advance(),
                }
            }
            Phase::Idle | Phase::Done => {}
        }
    }
}

#[cfg(test)]
#[path = "step_edge_tests.rs"]
mod edge_tests;
#[cfg(test)]
#[path = "step_props.rs"]
mod props;
#[cfg(test)]
#[path = "step_tests.rs"]
mod tests;
