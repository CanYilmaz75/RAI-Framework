//! Der Treiber: ruft `step` auf, speichert und verteilt die Events, führt die Effekte aus.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use can_core::{
    Approver, Assembler, Chunk, Decision, Envelope, Event, FinishReason, Message, Phase,
    ProgressSink, ProviderError, Request, RunId, RunState, SCHEMA_VERSION, SessionId, ToolCall,
    ToolCtx, ToolOutput,
};
use futures::StreamExt;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::sync::CancellationToken;

use crate::agent::Inner;
use crate::exec::{self, ToolRun};
use crate::run::{Command, Outcome, RunError};
use crate::step::{Effect, Input, step};

/// So oft wird ein mittendrin abgebrochener Antwort-Strom neu angefragt.
const MAX_STREAM_RESTARTS: u32 = 2;

fn is_transient(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Network(_) | ProviderError::Overloaded | ProviderError::RateLimited { .. }
    )
}

/// So viele Tools laufen höchstens gleichzeitig.
const MAX_PARALLEL_TOOLS: usize = 4;

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
    pub(crate) async fn run(mut self, first: Input) -> Result<Outcome, RunError> {
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
            } => Input::ModelDone {
                message: redactor.value(message),
                usage,
                stop,
            },
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

    /// Fragt das Modell. Bricht der Strom mittendrin mit einem vorübergehenden Fehler ab, wird
    /// die Teilantwort verworfen und der Schritt neu angefragt.
    async fn call_model(&mut self, request: Request) -> Input {
        let mut restarts = 0;
        loop {
            match self.stream_once(request.clone()).await {
                Err((error, true)) if restarts < MAX_STREAM_RESTARTS && is_transient(&error) => {
                    tracing::warn!(%error, "Modellantwort brach ab; der Schritt wird neu angefragt");
                    restarts += 1;
                }
                Err((error, _)) => return Input::ModelFailed(error),
                Ok(input) => return input,
            }
        }
    }

    /// Ein Versuch. Im Fehlerfall sagt der zweite Wert, ob schon Chunks angekommen waren.
    async fn stream_once(&mut self, request: Request) -> Result<Input, (ProviderError, bool)> {
        let mut stream = self
            .agent
            .provider
            .stream(request)
            .await
            .map_err(|error| (error, false))?;
        let mut assembler = Assembler::default();
        let mut received = false;
        while let Some(item) = stream.next().await {
            let chunk = item.map_err(|error| (error, received))?;
            received = true;
            if let Chunk::TextDelta { text } = &chunk {
                self.send_delta(text);
            }
            assembler.push(chunk).map_err(|error| (error, false))?;
        }
        let (message, usage, stop) = assembler.finish().map_err(|error| (error, false))?;
        Ok(Input::ModelDone {
            message,
            usage,
            stop,
        })
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
        let decision = match self.agent.policy.check(call, tool.spec()) {
            Decision::Ask if !self.interactive => Decision::Deny,
            decision => decision,
        };
        Input::PolicyDecided {
            call_id,
            decision,
            by: Approver::Rule,
        }
    }

    /// Lässt das Modell den alten Verlauf zusammenfassen; Deltas gehen dabei nicht an den Kanal.
    async fn summarize(&self, up_to_seq: u64, request: Request) -> Input {
        let mut stream = match self.agent.provider.stream(request).await {
            Ok(stream) => stream,
            Err(error) => return Input::CompactFailed(error.to_string()),
        };
        let mut assembler = Assembler::default();
        while let Some(item) = stream.next().await {
            if let Err(error) = item.and_then(|chunk| assembler.push(chunk)) {
                return Input::CompactFailed(error.to_string());
            }
        }
        match assembler.finish() {
            Ok((message, ..)) => Input::Compacted {
                up_to_seq,
                summary: Message::assistant(message.text()),
            },
            Err(error) => Input::CompactFailed(error.to_string()),
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
        let cx = ToolCtx {
            session: self.session,
            call_id,
            workdir: self.agent.workdir.clone(),
            cancel: self.cancel.child_token(),
            clock: self.agent.clock.clone(),
            progress: ProgressSink::noop(),
            secrets: self.agent.secrets.clone(),
        };
        match exec::run_tool(tool, call, cx, &self.cancel, self.remaining()).await {
            ToolRun::Done { output, is_error } => Input::ToolDone {
                call_id,
                output,
                is_error,
            },
            ToolRun::Fatal(message) => Input::ToolFatal { call_id, message },
            ToolRun::Cancelled => Input::Cancel,
            ToolRun::Deadline => Input::Tick(self.agent.clock.now()),
        }
    }

    /// Restzeit bis zum Zeitlimit des Laufs.
    fn remaining(&self) -> Duration {
        match self.state.deadline() {
            Some(deadline) => Duration::from_millis(deadline.millis_since(self.agent.clock.now())),
            None => Duration::from_secs(u64::from(u32::MAX)),
        }
    }

    /// Flüchtig: bei vollem Kanal wird der Text gesammelt statt zu blockieren.
    fn send_delta(&mut self, text: &str) {
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
