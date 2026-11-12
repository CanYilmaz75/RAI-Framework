//! Die reine Schrittfunktion: entscheidet, macht aber selbst nie Ein- oder Ausgabe.

use can_core::{
    Approver, CallId, CallStatus, Decision, Effect as ToolEffect, Event, FinishReason, LimitKind,
    Message, Phase, ProviderError, RunState, StopReason, ToolCall, ToolOutput, Usage,
};

#[cfg(test)]
use can_core::Timestamp;

pub use crate::step_types::{Effect, Input, Step};

use crate::context::{KEEP_ROUNDS, build_request, compaction_due, plan_compaction};

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
        (Phase::AwaitModel, Input::ModelFailed(ProviderError::ContextTooLong { limit })) => {
            // Genau ein weiterer Versuch pro Verdichtung: gibt es nichts mehr zu verdichten, endet der Lauf.
            let plan = plan_compaction(state, KEEP_ROUNDS).or_else(|| plan_compaction(state, 1));
            match plan {
                Some((up_to_seq, request)) => {
                    em.out.effects.push(Effect::Compact { up_to_seq, request })
                }
                None => em.finish(FinishReason::Error {
                    message: ProviderError::ContextTooLong { limit }.to_string(),
                }),
            }
        }
        (Phase::AwaitModel, Input::ModelFailed(error)) => {
            em.finish(FinishReason::Error {
                message: error.to_string(),
            });
        }
        (Phase::AwaitModel, Input::Compacted { up_to_seq, summary })
            if em.compaction_fits(up_to_seq) =>
        {
            em.emit(Event::Compacted { up_to_seq, summary });
            em.out
                .effects
                .push(Effect::CallModel(build_request(&em.state)));
        }
        (Phase::AwaitModel, Input::CompactFailed(message)) => {
            em.finish(FinishReason::Error {
                message: format!("Verdichtung fehlgeschlagen: {message}"),
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
            em.tool_done(call_id, output, is_error);
        }
        (Phase::AwaitPolicy, Input::ApprovedWithEdit { call_id, input })
            if em.unresolved(call_id) =>
        {
            em.emit(Event::ToolArgsEdited { call_id, input });
            em.emit(Event::ApprovalResolved {
                call_id,
                decision: Decision::Allow,
                by: Approver::Human,
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
pub(crate) struct Emitter {
    pub(crate) state: RunState,
    pub(crate) out: Step,
}

impl Emitter {
    pub(crate) fn emit(&mut self, event: Event) {
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

    /// Eine Verdichtung muss über die letzte hinausgehen und darf nichts Künftiges ersetzen.
    fn compaction_fits(&self, up_to_seq: u64) -> bool {
        let previous = self
            .state
            .compaction
            .as_ref()
            .map(|compaction| compaction.up_to_seq);
        up_to_seq < self.state.next_seq && previous.is_none_or(|previous| up_to_seq > previous)
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
        self.call_model_or_compact();
    }

    /// Ist das Kontextfenster zu 80 % voll, wird vor der Anfrage erst verdichtet.
    fn call_model_or_compact(&mut self) {
        let effect = match compaction_due(&self.state) {
            Some((up_to_seq, request)) => Effect::Compact { up_to_seq, request },
            None => Effect::CallModel(build_request(&self.state)),
        };
        self.out.effects.push(effect);
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

    fn read_only(&self, call: &ToolCall) -> bool {
        self.state
            .config
            .tool(&call.name)
            .is_some_and(|spec| spec.effect == ToolEffect::ReadOnly)
    }

    /// Sobald alle Aufrufe entschieden sind: Tools in Modellreihenfolge starten, danach das
    /// Modell erneut fragen. Aufeinanderfolgende lesende Tools starten gemeinsam und laufen
    /// parallel; alles andere läuft einzeln. Solange ein Tool läuft, passiert nichts.
    pub(crate) fn advance(&mut self) {
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
        let waiting = self
            .state
            .pending
            .iter()
            .filter(|pending| pending.status == CallStatus::Allowed);
        let waiting: Vec<ToolCall> = waiting.map(|pending| pending.call.clone()).collect();
        let Some(first) = waiting.first() else {
            return;
        };
        let batch = if self.read_only(first) {
            waiting
                .iter()
                .take_while(|call| self.read_only(call))
                .count()
        } else {
            1
        };
        for call in waiting.into_iter().take(batch) {
            self.emit(Event::ToolStarted { call_id: call.id });
            self.out.effects.push(Effect::RunTool(call));
        }
    }

    /// Wiederaufnahme: was im Log begonnen, aber nicht beendet wurde, wird nachgeholt.
    fn resume(&mut self) {
        match self.state.phase {
            Phase::AwaitModel => self.call_model_or_compact(),
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
                    .filter(|p| p.status == CallStatus::Started);
                let started: Vec<ToolCall> = started.map(|pending| pending.call.clone()).collect();
                for call in started {
                    if self
                        .state
                        .config
                        .tool(&call.name)
                        .is_some_and(|spec| spec.idempotent)
                    {
                        self.out.effects.push(Effect::RunTool(call));
                    } else {
                        // Nicht idempotent: nie unbemerkt doppelt ausführen.
                        self.tool_error(call.id, "Unterbrochen, Ausgang unbekannt.");
                    }
                }
                self.advance();
            }
            Phase::Idle | Phase::Done => {}
        }
    }
}

#[cfg(test)]
#[path = "step_context_tests.rs"]
mod context_tests;
#[cfg(test)]
#[path = "step_edge_tests.rs"]
mod edge_tests;
#[cfg(test)]
#[path = "step_props.rs"]
mod props;
#[cfg(test)]
#[path = "step_tests.rs"]
mod tests;
