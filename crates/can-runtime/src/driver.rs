//! Der Treiber: ruft `step` auf, speichert und verteilt die Events, führt die Effekte aus.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use can_core::{
    Approver, Decision, Envelope, Event, FinishReason, Phase, ProgressSink, RunId, RunState,
    SCHEMA_VERSION, SessionId, ToolCall, ToolCtx, ToolOutput,
};
use futures::StreamExt;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::sync::CancellationToken;

use crate::agent::Inner;
use crate::exec::{self, ToolRun};
use crate::run::{Command, Outcome, RunError};
use crate::step::{Effect, Input, step};

/// So viele Tools laufen höchstens gleichzeitig.
const MAX_PARALLEL_TOOLS: usize = 4;

/// Verlängert die Sperre, solange der Lauf lebt. Geht sie verloren, endet nur die Verlängerung;
/// der nächste Schreibversuch scheitert dann am Konflikt.
async fn heartbeat(agent: Arc<Inner>, session: SessionId) {
    loop {
        agent.clock.sleep(agent.lease / 3).await;
        if let Err(error) = agent
            .store
            .acquire(session, &agent.owner, agent.clock.now(), agent.lease)
            .await
        {
            tracing::warn!(%error, "Sperre der Sitzung nicht verlängert");
            return;
        }
    }
}

pub(crate) struct Driver {
    pub(crate) agent: Arc<Inner>,
    pub(crate) session: SessionId,
    pub(crate) run_id: RunId,
    /// Ohne Mensch wird `Ask` zu `Deny`.
    pub(crate) interactive: bool,
    pub(crate) state: RunState,
    pub(crate) events: mpsc::Sender<Envelope>,
    pub(crate) commands: mpsc::Receiver<Command>,
    pub(crate) cancel: CancellationToken,
    /// Text-Deltas, die bei vollem Kanal zusammengefasst wurden.
    pub(crate) delta: String,
}

impl Driver {
    /// Wiederholt: `step` aufrufen, Events speichern, anwenden und verteilen, Effekte ausführen.
    /// Führt den Lauf unter der Sperre der Sitzung: nehmen, regelmäßig verlängern, freigeben.
    pub(crate) async fn run(mut self, first: Input) -> Result<Outcome, RunError> {
        let (agent, session) = (self.agent.clone(), self.session);
        agent
            .store
            .acquire(session, &agent.owner, agent.clock.now(), agent.lease)
            .await?;
        let heartbeat = tokio::spawn(heartbeat(agent.clone(), session));
        let outcome = self.drive(first).await;
        heartbeat.abort();
        if let Err(error) = agent.store.release(session, &agent.owner).await {
            tracing::warn!(%error, "Sperre der Sitzung nicht freigegeben");
        }
        outcome
    }

    /// Wiederholt: `step` aufrufen, Events speichern, anwenden und verteilen, Effekte ausführen.
    async fn drive(&mut self, first: Input) -> Result<Outcome, RunError> {
        let mut queue = VecDeque::from([first]);
        loop {
            let input = match queue.pop_front() {
                Some(input) => input,
                None if matches!(self.state.phase, Phase::Done | Phase::Idle) => break,
                None => self.wait().await,
            };
            let step = step(&self.state, self.redact(input));
            self.commit(step.events).await?;
            let (tools, others): (Vec<Effect>, Vec<Effect>) = step
                .effects
                .into_iter()
                .partition(|effect| matches!(effect, Effect::RunTool(_)));
            for effect in others {
                queue.push_back(self.execute(effect).await);
            }
            queue.extend(self.run_tools(tools).await);
        }
        Ok(Outcome {
            reason: self.state.finish.clone().unwrap_or(FinishReason::Cancelled),
            text: self.state.last_assistant_text(),
            usage: self.state.usage,
            steps: self.state.step,
        })
    }

    /// Schwärzt, was von Modell und Tools kommt, bevor die Schrittfunktion daraus Events und
    /// die nächste Anfrage baut.
    fn redact(&self, input: Input) -> Input {
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
            other => other,
        }
    }

    /// Speichert die Events, bevor irgendeine weitere Wirkung nach außen passiert.
    async fn commit(&mut self, events: Vec<Event>) -> Result<(), RunError> {
        if events.is_empty() {
            return Ok(());
        }
        let ts = self.agent.clock.now();
        let envelopes: Vec<Envelope> = (self.state.next_seq..)
            .zip(events)
            .map(|(seq, event)| Envelope {
                seq,
                run_id: self.run_id,
                ts,
                schema: SCHEMA_VERSION,
                event: self.agent.redactor.event(event),
            })
            .collect();
        self.agent.store.append(self.session, &envelopes).await?;
        self.flush_delta().await?;
        for envelope in envelopes {
            tracing::debug!(seq = envelope.seq, kind = envelope.event.kind(), "event");
            self.state.apply_envelope(&envelope);
            self.events
                .send(envelope)
                .await
                .map_err(|_| RunError::Closed)?;
        }
        Ok(())
    }

    /// Wartet auf eine Freigabe, den Abbruch oder das Zeitlimit.
    async fn wait(&mut self) -> Input {
        let clock = self.agent.clock.clone();
        let remaining = self.remaining();
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Input::Cancel,
            command = self.commands.recv() => match command {
                Some(Command::Resolve { call_id, decision }) => {
                    Input::PolicyDecided { call_id, decision, by: Approver::Human }
                }
                None => Input::Cancel,
            },
            _ = clock.sleep(remaining) => Input::Tick(clock.now()),
        }
    }

    async fn execute(&mut self, effect: Effect) -> Input {
        match effect {
            Effect::CallModel(request) => {
                let (clock, cancel) = (self.agent.clock.clone(), self.cancel.clone());
                let remaining = self.remaining();
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Input::Cancel,
                    input = self.call_model(request) => input,
                    _ = clock.sleep(remaining) => Input::Tick(clock.now()),
                }
            }
            Effect::CheckPolicy(call) => self.check_policy(&call),
            Effect::RunTool(call) => self.run_tool(&call).await,
            Effect::Compact { up_to_seq, request } => {
                let (clock, cancel) = (self.agent.clock.clone(), self.cancel.clone());
                let remaining = self.remaining();
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Input::Cancel,
                    input = self.summarize(up_to_seq, request) => input,
                    _ = clock.sleep(remaining) => Input::Tick(clock.now()),
                }
            }
        }
    }

    /// Reihenfolge: Tool nachschlagen, Argumente prüfen, dann die Policy fragen.
    fn check_policy(&self, call: &ToolCall) -> Input {
        let call_id = call.id;
        let Some(tool) = self.agent.tools.get(&call.name) else {
            return Input::ToolRejected {
                call_id,
                message: format!("Unbekanntes Tool: {}", call.name),
            };
        };
        if let Err(problem) = can_core::schema::validate(&tool.spec().input_schema.0, &call.input) {
            return Input::ToolRejected {
                call_id,
                message: format!("Ungültige Argumente: {problem}"),
            };
        }
        let mut decision = self.agent.policy.check(call, tool.spec());
        // Schutz vor Datenabfluss: nach vertraulichen Daten wird jedes externe Tool strenger behandelt.
        if let Some(guard) = self.state.config.exfiltration_guard
            && self.state.confidential_seen
            && tool.spec().effect == can_core::Effect::External
        {
            decision = decision.max(guard);
        }
        let decision = match decision {
            Decision::Ask if !self.interactive => Decision::Deny,
            decision => decision,
        };
        Input::PolicyDecided {
            call_id,
            decision,
            by: Approver::Rule,
        }
    }

    /// Führt die gemeinsam gestarteten Tools aus, höchstens `MAX_PARALLEL_TOOLS` gleichzeitig.
    /// Die Ergebnisse kommen in der Reihenfolge der Aufrufe zurück, nicht der Fertigstellung.
    async fn run_tools(&self, effects: Vec<Effect>) -> Vec<Input> {
        let calls = effects.into_iter().filter_map(|effect| match effect {
            Effect::RunTool(call) => Some(call),
            _ => None,
        });
        futures::stream::iter(calls.map(|call| async move { self.run_tool(&call).await }))
            .buffered(MAX_PARALLEL_TOOLS)
            .collect()
            .await
    }

    async fn run_tool(&self, call: &ToolCall) -> Input {
        let call_id = call.id;
        let Some(tool) = self.agent.tools.get(&call.name) else {
            let output = ToolOutput::text(format!("Unbekanntes Tool: {}", call.name));
            return Input::ToolDone {
                call_id,
                output,
                is_error: true,
            };
        };
        let workdir = match self.session_workdir() {
            Ok(workdir) => workdir,
            Err(error) => {
                let message = format!("Arbeitsverzeichnis der Sitzung nicht nutzbar: {error}");
                return Input::ToolFatal { call_id, message };
            }
        };
        let cx = ToolCtx {
            session: self.session,
            call_id,
            workdir,
            cancel: self.cancel.child_token(),
            clock: self.agent.clock.clone(),
            progress: ProgressSink::noop(),
            secrets: self.agent.secrets.clone(),
        };
        let max_output_bytes = tool.spec().max_output_bytes;
        match exec::run_tool(tool, call, cx, &self.cancel, self.remaining()).await {
            ToolRun::Done { output, is_error } => {
                let output = self.shorten(output, max_output_bytes).await;
                Input::ToolDone {
                    call_id,
                    output,
                    is_error,
                }
            }
            ToolRun::Fatal(message) => Input::ToolFatal { call_id, message },
            ToolRun::Cancelled => Input::Cancel,
            ToolRun::Deadline => Input::Tick(self.agent.clock.now()),
        }
    }

    /// Kürzt eine zu große Ausgabe für das Modell. Die volle Ausgabe geht als Blob in den Store;
    /// das Ergebnis nennt ihre Inhaltsadresse.
    async fn shorten(&self, output: ToolOutput, max_bytes: usize) -> ToolOutput {
        if exec::text_bytes(&output) <= max_bytes {
            return output;
        }
        let full = self.agent.redactor.text(&output.as_text()).into_bytes();
        let size = full.len();
        let mut shortened = exec::truncate(output, max_bytes);
        let note = match self.agent.store.put_blob(full).await {
            Ok(digest) => format!("\n[Volle Ausgabe: {size} Bytes, im Store als Blob {digest}]"),
            Err(error) => format!("\n[Volle Ausgabe: {size} Bytes, nicht gespeichert: {error}]"),
        };
        shortened.0.push(can_core::OutputBlock::Text(note));
        shortened
    }

    /// Das Arbeitsverzeichnis für Tools dieser Sitzung; bei getrennten Verzeichnissen wird es
    /// bei Bedarf angelegt.
    fn session_workdir(&self) -> std::io::Result<std::path::PathBuf> {
        if !self.agent.isolate_workdir {
            return Ok(self.agent.workdir.clone());
        }
        let own = self.agent.workdir.join(self.session.to_string());
        std::fs::create_dir_all(&own)?;
        own.canonicalize()
    }

    /// Restzeit bis zum Zeitlimit des Laufs.
    fn remaining(&self) -> Duration {
        match self.state.deadline() {
            Some(deadline) => Duration::from_millis(deadline.millis_since(self.agent.clock.now())),
            None => Duration::from_secs(u64::from(u32::MAX)),
        }
    }

    /// Flüchtig: bei vollem Kanal wird der Text gesammelt statt zu blockieren.
    pub(crate) fn send_delta(&mut self, text: &str) {
        self.delta.push_str(&self.agent.redactor.text(text));
        let envelope = self.delta_envelope();
        match self.events.try_send(envelope) {
            Ok(()) | Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(envelope)) => {
                if let Event::TextDelta { text, .. } = envelope.event {
                    self.delta = text;
                }
            }
        }
    }

    async fn flush_delta(&mut self) -> Result<(), RunError> {
        if self.delta.is_empty() {
            return Ok(());
        }
        let envelope = self.delta_envelope();
        self.events
            .send(envelope)
            .await
            .map_err(|_| RunError::Closed)
    }

    fn delta_envelope(&mut self) -> Envelope {
        Envelope {
            seq: self.state.next_seq.saturating_sub(1),
            run_id: self.run_id,
            ts: self.agent.clock.now(),
            schema: SCHEMA_VERSION,
            event: Event::TextDelta {
                step: self.state.step,
                text: std::mem::take(&mut self.delta),
            },
        }
    }
}
