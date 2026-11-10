//! Der Agent und sein Builder: die Stelle, an der alle Teile zusammenkommen.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use can_core::{
    ByEffect, Clock, Limits, MemoryStore, Message, Phase, Policy, Provider, RunConfig, RunId,
    RunState, SecretsHandle, SessionId, Store, Tool,
};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::clock::SystemClock;
use crate::driver::Driver;
use crate::registry::Registry;
use crate::run::{Outcome, Run, RunError};
use crate::step::Input;

/// Plätze im Event-Kanal; dauerhafte Events warten auf einen langsamen Empfänger.
const EVENT_CHANNEL: usize = 256;

pub(crate) struct Inner {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) tools: Registry,
    pub(crate) policy: Arc<dyn Policy>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: RunConfig,
    pub(crate) workdir: PathBuf,
    pub(crate) secrets: SecretsHandle,
}

/// Ein fertig konfigurierter Agent. Billig zu klonen.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

impl Agent {
    /// Beginnt die Konfiguration.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Die Konfiguration, mit der Zustände gefaltet werden.
    pub fn config(&self) -> &RunConfig {
        &self.inner.config
    }

    /// Startet einen neuen Lauf in der Sitzung und liefert den Event-Strom.
    pub async fn run(
        &self,
        session: SessionId,
        input: impl Into<Message>,
    ) -> Result<Run, RunError> {
        self.start(session, Some(input.into()), true).await
    }

    /// Setzt einen unterbrochenen Lauf am letzten gespeicherten Zustand fort.
    pub async fn resume(&self, session: SessionId) -> Result<Run, RunError> {
        self.start(session, None, true).await
    }

    /// Wie [`Agent::run`], aber ohne Mensch: jede Rückfrage wird zur Ablehnung.
    pub async fn run_unattended(
        &self,
        session: SessionId,
        input: impl Into<Message>,
    ) -> Result<Run, RunError> {
        self.start(session, Some(input.into()), false).await
    }

    /// Kurzform: liefert nur das Ergebnis und behandelt `Ask` als `Deny`.
    pub async fn complete(
        &self,
        session: SessionId,
        input: impl Into<Message>,
    ) -> Result<Outcome, RunError> {
        self.start(session, Some(input.into()), false)
            .await?
            .outcome()
            .await
    }

    async fn start(
        &self,
        session: SessionId,
        input: Option<Message>,
        interactive: bool,
    ) -> Result<Run, RunError> {
        let log = self.inner.store.load(session).await?;
        let state = RunState::fold(self.inner.config.clone(), &log);
        let running = !matches!(state.phase, Phase::Idle | Phase::Done);
        let (first, run_id) = match input {
            Some(_) if running => return Err(RunError::Interrupted),
            Some(message) => (Input::Start(message), RunId::new()),
            None if !running => return Err(RunError::NothingToResume),
            None => (
                Input::Resume,
                state.run_id.ok_or(RunError::NothingToResume)?,
            ),
        };
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL);
        let (command_tx, command_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let driver = Driver {
            agent: self.inner.clone(),
            session,
            run_id,
            interactive,
            state,
            events: event_tx,
            commands: command_rx,
            cancel: cancel.clone(),
            delta: String::new(),
        };
        let span = tracing::info_span!("run", session = %session, run = %run_id);
        let task = tokio::spawn(driver.run(first).instrument(span));
        Ok(Run::new(event_rx, command_tx, cancel, task))
    }
}

/// Alle Fehler einer Konfiguration, gesammelt.
#[derive(Debug, PartialEq, Eq)]
pub struct BuildError {
    pub problems: Vec<String>,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ungültige Agent-Konfiguration: {}",
            self.problems.join("; ")
        )
    }
}

impl std::error::Error for BuildError {}

/// Builder für [`Agent`]. `build()` prüft alles und meldet alle Fehler auf einmal.
#[derive(Default)]
pub struct AgentBuilder {
    provider: Option<Arc<dyn Provider>>,
    model: Option<String>,
    system: Option<String>,
    tools: Vec<Arc<dyn Tool>>,
    policy: Option<Arc<dyn Policy>>,
    store: Option<Arc<dyn Store>>,
    clock: Option<Arc<dyn Clock>>,
    limits: Limits,
    max_output: Option<u32>,
    extra: Map<String, Value>,
    workdir: Option<PathBuf>,
    secrets: SecretsHandle,
}

impl AgentBuilder {
    /// Der Modellanbieter. Pflicht.
    pub fn provider(mut self, provider: impl Provider + 'static) -> Self {
        self.provider = Some(Arc::new(provider));
        self
    }

    /// Wie [`AgentBuilder::provider`], für einen schon geteilten Provider.
    pub fn provider_arc(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Name des Modells. Pflicht.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// System-Prompt.
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Fügt ein Tool hinzu.
    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Wie [`AgentBuilder::tool`], für ein schon geteiltes Tool.
    pub fn tool_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Policy; Vorgabe ist [`ByEffect`]: Lesen erlaubt, alles andere fragt nach.
    pub fn policy(mut self, policy: impl Policy + 'static) -> Self {
        self.policy = Some(Arc::new(policy));
        self
    }

    /// Store; Vorgabe ist ein [`MemoryStore`].
    pub fn store(mut self, store: impl Store + 'static) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// Wie [`AgentBuilder::store`], für einen schon geteilten Store.
    pub fn store_arc(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Uhr; Vorgabe ist die Systemzeit.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Harte Limits pro Lauf.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Obergrenze für Ausgabe-Token pro Modellantwort.
    pub fn max_output(mut self, max_output: u32) -> Self {
        self.max_output = Some(max_output);
        self
    }

    /// Anbieterspezifischer Parameter, der unverändert durchgereicht wird.
    pub fn extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    /// Arbeitsverzeichnis der Tools; Vorgabe ist das aktuelle Verzeichnis.
    pub fn workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    /// Schlüssel, die Tools sehen dürfen.
    pub fn secrets(mut self, secrets: SecretsHandle) -> Self {
        self.secrets = secrets;
        self
    }

    /// Prüft Tool-Namen, Modellfähigkeiten, Limits und Arbeitsverzeichnis.
    pub fn build(self) -> Result<Agent, BuildError> {
        let mut problems = Vec::new();
        let mut tools = Registry::new();
        for tool in self.tools {
            if let Err(problem) = tools.insert(tool) {
                problems.push(problem);
            }
        }
        let model = self.model.unwrap_or_default();
        if model.trim().is_empty() {
            problems.push("kein Modell angegeben".to_owned());
        }
        match &self.provider {
            None => problems.push("kein Provider angegeben".to_owned()),
            Some(provider) if !tools.is_empty() && !provider.capabilities(&model).tools => {
                problems.push(format!("das Modell {model} unterstützt keine Tools"));
            }
            Some(_) => {}
        }
        if self.limits.max_steps == 0 {
            problems.push("max_steps muss größer als null sein".to_owned());
        }
        if self.limits.timeout.is_zero() {
            problems.push("timeout muss größer als null sein".to_owned());
        }
        let workdir = self.workdir.map(Ok).unwrap_or_else(std::env::current_dir);
        let workdir = match workdir.and_then(|path| path.canonicalize()) {
            Ok(path) => path,
            Err(error) => {
                problems.push(format!("Arbeitsverzeichnis nicht nutzbar: {error}"));
                PathBuf::new()
            }
        };
        let (Some(provider), true) = (self.provider, problems.is_empty()) else {
            return Err(BuildError { problems });
        };
        let capabilities = provider.capabilities(&model);
        let config = RunConfig {
            model,
            system: self.system,
            tools: tools.specs(),
            limits: self.limits,
            max_output: self.max_output,
            extra: self.extra,
            context_window: capabilities.context_window,
            reserve_output: self.max_output.unwrap_or(capabilities.max_output),
        };
        Ok(Agent {
            inner: Arc::new(Inner {
                provider,
                tools,
                policy: self.policy.unwrap_or_else(|| Arc::new(ByEffect)),
                store: self.store.unwrap_or_else(|| Arc::new(MemoryStore::new())),
                clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
                config,
                workdir,
                secrets: self.secrets,
            }),
        })
    }
}
