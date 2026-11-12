//! Abschluss eines Tool-Aufrufs samt seiner Nebenwirkungen auf den Zustand: Hinweis auf
//! geänderte Argumente und der Plan des Agenten.

use can_core::{CallId, Event, OutputBlock, PLAN_TOOL, Todo, ToolOutput};
use serde::Deserialize;

use crate::step::Emitter;

#[derive(Deserialize)]
struct PlanArgs {
    todos: Vec<Todo>,
}

impl Emitter {
    /// Hält das Ergebnis fest. Hatte ein Mensch die Argumente geändert, erfährt das Modell
    /// davon; war es das Planungs-Tool, wird sein Argument zum Plan der Sitzung.
    pub(crate) fn tool_done(&mut self, call_id: CallId, mut output: ToolOutput, is_error: bool) {
        let pending = self.state.pending_call(call_id).cloned();
        if let Some(pending) = pending.as_ref().filter(|pending| pending.edited) {
            let note = format!(
                "[Ein Mensch hat die Argumente vor der Ausführung geändert in: {}]\n",
                pending.call.input
            );
            output.0.insert(0, OutputBlock::Text(note));
        }
        self.emit(Event::ToolFinished {
            call_id,
            output,
            is_error,
        });
        let plan = pending
            .filter(|pending| !is_error && pending.call.name == PLAN_TOOL)
            .and_then(|pending| serde_json::from_value::<PlanArgs>(pending.call.input).ok());
        if let Some(plan) = plan {
            self.emit(Event::PlanUpdated { todos: plan.todos });
        }
        self.advance();
    }
}
