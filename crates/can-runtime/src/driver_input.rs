//! Eingaben von außen in den Treiber: Schwärzung und geänderte Freigaben.

use crate::driver::Driver;
use crate::step::Input;

impl Driver {
    /// Geänderte Argumente einer Freigabe; passen sie nicht zum Schema des Tools, bleibt die
    /// Rückfrage offen.
    pub(crate) fn edited(&self, call_id: can_core::CallId, input: serde_json::Value) -> Input {
        let name = self
            .state
            .pending_call(call_id)
            .map(|pending| pending.call.name.clone());
        let tool = name.and_then(|name| self.agent.tools.get(&name));
        let valid = tool.is_some_and(|tool| {
            can_core::schema::validate(&tool.spec().input_schema.0, &input).is_ok()
        });
        if valid {
            return Input::ApprovedWithEdit { call_id, input };
        }
        tracing::warn!(%call_id, "geänderte Argumente passen nicht zum Schema; die Rückfrage bleibt offen");
        Input::Tick(self.agent.clock.now())
    }

    /// Schwärzt, was von Modell und Tools kommt, bevor die Schrittfunktion daraus Events und
    /// die nächste Anfrage baut.
    pub(crate) fn redact(&self, input: Input) -> Input {
        let redactor = &self.agent.redactor;
        match input {
            Input::ModelDone {
                message,
                usage,
                stop,
            } => {
                // Der Inhaltsfilter sieht die Antwort, bevor sie in den Verlauf kommt.
                let blocked = self
                    .agent
                    .moderator
                    .as_ref()
                    .and_then(|moderator| moderator.check(&message.text()).err());
                match blocked {
                    Some(reason) => {
                        Input::ModelFailed(can_core::ProviderError::InvalidRequest(format!(
                            "Die Antwort des Modells wurde vom Inhaltsfilter abgelehnt: {reason}"
                        )))
                    }
                    None => Input::ModelDone {
                        message: redactor.value(message),
                        usage,
                        stop,
                    },
                }
            }
            Input::ToolDone {
                call_id,
                output,
                is_error,
            } => Input::ToolDone {
                call_id,
                output: redactor.value(output),
                is_error,
            },
            Input::ToolRejected { call_id, message } => Input::ToolRejected {
                call_id,
                message: redactor.text(&message),
            },
            Input::ToolFatal { call_id, message } => Input::ToolFatal {
                call_id,
                message: redactor.text(&message),
            },
            Input::Compacted { up_to_seq, summary } => Input::Compacted {
                up_to_seq,
                summary: redactor.value(summary),
            },
            Input::Inject(message) => Input::Inject(redactor.value(message)),
            Input::ApprovedWithEdit { call_id, input } => Input::ApprovedWithEdit {
                call_id,
                input: redactor.value(input),
            },
            other => other,
        }
    }
}
