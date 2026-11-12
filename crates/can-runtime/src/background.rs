//! Hintergrund-Sub-Agenten: ein Agent bekommt eine Aufgabe in einer eigenen Sitzung und arbeitet
//! sie ab, während der aufrufende Lauf weitermacht.
//!
//! Fünf Tools steuern ihn, jeweils mit dem Namen des Agenten als Präfix: `<name>__start`,
//! `<name>__check`, `<name>__message`, `<name>__cancel` und `<name>__list`. Eine Aufgabe gehört
//! der Sitzung, die sie gestartet hat; andere Sitzungen sehen sie nicht. Weil der Sub-Agent eine
//! eigene Sitzung hat, behält er seinen Verlauf: eine Nachricht an eine fertige Aufgabe setzt sie
//! mit vollem Kontext fort.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use can_core::{Effect, FinishReason, SessionId, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::agent::Agent;
use crate::run::RunController;

/// Länger wartet `check` nicht auf das Ende einer Aufgabe.
const MAX_WAIT: Duration = Duration::from_secs(60);

/// Stand einer Aufgabe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Done { text: String },
    Failed { reason: String },
}

struct Task {
    owner: SessionId,
    prompt: String,
    status: TaskStatus,
    controller: Option<RunController>,
    changed: Arc<Notify>,
}

/// Der Hintergrund-Agent und seine laufenden Aufgaben.
#[derive(Clone)]
pub struct BackgroundAgents {
    name: String,
    description: String,
    agent: Agent,
    tasks: Arc<Mutex<HashMap<SessionId, Task>>>,
}

fn for_model(message: impl Into<String>) -> ToolError {
    ToolError::ForModel(message.into())
}

fn status_text(id: SessionId, task: &Task) -> String {
    let short: String = task.prompt.chars().take(80).collect();
    match &task.status {
        TaskStatus::Running => format!("{id}: läuft ({short})"),
        TaskStatus::Done { text } => format!("{id}: fertig\n{text}"),
        TaskStatus::Failed { reason } => format!("{id}: fehlgeschlagen: {reason}"),
    }
}

impl BackgroundAgents {
    /// Hintergrund-Agent unter `name`; `description` sagt dem Modell, wofür er gut ist.
    pub fn new(name: impl Into<String>, description: impl Into<String>, agent: Agent) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            agent,
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Die fünf Tools zur Steuerung.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        ["start", "check", "message", "cancel", "list"]
            .into_iter()
            .map(|action| Arc::new(Control::new(self.clone(), action)) as Arc<dyn Tool>)
            .collect()
    }

    /// Stand einer Aufgabe, aus Sicht der Sitzung `owner`.
    pub fn status(&self, owner: SessionId, id: SessionId) -> Option<TaskStatus> {
        let tasks = self.tasks.lock().ok()?;
        tasks
            .get(&id)
            .filter(|task| task.owner == owner)
            .map(|task| task.status.clone())
    }

    /// Startet einen Lauf in der Sitzung der Aufgabe und hält ihr Ergebnis fest, sobald er endet.
    async fn launch(
        &self,
        owner: SessionId,
        id: SessionId,
        prompt: String,
    ) -> Result<(), ToolError> {
        let run = self
            .agent
            .run_unattended(id, prompt.clone())
            .await
            .map_err(|error| for_model(format!("Hintergrund-Agent nicht startbar: {error}")))?;
        let changed = Arc::new(Notify::new());
        let task = Task {
            owner,
            prompt,
            status: TaskStatus::Running,
            controller: Some(run.controller()),
            changed: changed.clone(),
        };
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.insert(id, task);
        }
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            let mut run = run;
            let status = match run.outcome().await {
                Ok(outcome) if outcome.reason == FinishReason::Completed => {
                    TaskStatus::Done { text: outcome.text }
                }
                Ok(outcome) => TaskStatus::Failed {
                    reason: format!("{:?}", outcome.reason),
                },
                Err(error) => TaskStatus::Failed {
                    reason: error.to_string(),
                },
            };
            if let Ok(mut tasks) = tasks.lock()
                && let Some(task) = tasks.get_mut(&id)
            {
                task.status = status;
                task.controller = None;
            }
            changed.notify_waiters();
        });
        Ok(())
    }

    fn task_id(&self, args: &Value, owner: SessionId) -> Result<SessionId, ToolError> {
        let text = args["task_id"].as_str().unwrap_or_default();
        let id: SessionId = text
            .parse()
            .map_err(|_| for_model(format!("Ungültige Aufgabe: {text}")))?;
        match self.status(owner, id) {
            Some(_) => Ok(id),
            None => Err(for_model(format!("Unbekannte Aufgabe: {id}"))),
        }
    }

    async fn act(&self, action: &str, args: Value, cx: &ToolCtx) -> Result<String, ToolError> {
        let owner = cx.session;
        match action {
            "start" => {
                let task = args["task"].as_str().unwrap_or_default().trim().to_owned();
                if task.is_empty() {
                    return Err(for_model("`task` ist leer."));
                }
                let id = SessionId::new();
                self.launch(owner, id, task).await?;
                Ok(format!(
                    "Aufgabe {id} läuft im Hintergrund. Mit {}__check fragst du nach dem Stand.",
                    self.name
                ))
            }
            "check" => {
                let id = self.task_id(&args, owner)?;
                let wait =
                    Duration::from_secs(args["wait_seconds"].as_u64().unwrap_or(0)).min(MAX_WAIT);
                let changed = self
                    .tasks
                    .lock()
                    .ok()
                    .and_then(|tasks| tasks.get(&id).map(|task| task.changed.clone()));
                if !wait.is_zero()
                    && self.status(owner, id) == Some(TaskStatus::Running)
                    && let Some(changed) = changed
                {
                    let notified = changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.status(owner, id) == Some(TaskStatus::Running) {
                        tokio::select! {
                            _ = notified => {}
                            _ = cx.clock.sleep(wait) => {}
                            _ = cx.cancel.cancelled() => {}
                        }
                    }
                }
                let tasks = self
                    .tasks
                    .lock()
                    .map_err(|_| for_model("Aufgabenliste nicht lesbar."))?;
                Ok(tasks
                    .get(&id)
                    .map(|task| status_text(id, task))
                    .unwrap_or_default())
            }
            "message" => {
                let id = self.task_id(&args, owner)?;
                let message = args["message"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if message.is_empty() {
                    return Err(for_model("`message` ist leer."));
                }
                let controller = self
                    .tasks
                    .lock()
                    .ok()
                    .and_then(|tasks| tasks.get(&id).and_then(|task| task.controller.clone()));
                match controller {
                    // Läuft die Aufgabe noch, geht die Nachricht hinein.
                    Some(controller) if controller.inject(message.clone()).await.is_ok() => {
                        Ok(format!("Nachricht an die laufende Aufgabe {id} übergeben."))
                    }
                    // Sonst setzt ein neuer Lauf die Aufgabe mit ihrem ganzen Verlauf fort.
                    _ => {
                        self.launch(owner, id, message).await?;
                        Ok(format!("Aufgabe {id} wird mit der Nachricht fortgesetzt."))
                    }
                }
            }
            "cancel" => {
                let id = self.task_id(&args, owner)?;
                let controller = self
                    .tasks
                    .lock()
                    .ok()
                    .and_then(|tasks| tasks.get(&id).and_then(|task| task.controller.clone()));
                match controller {
                    Some(controller) => {
                        controller.cancel();
                        Ok(format!("Aufgabe {id} wird abgebrochen."))
                    }
                    None => Ok(format!("Aufgabe {id} läuft nicht mehr.")),
                }
            }
            _ => {
                let tasks = self
                    .tasks
                    .lock()
                    .map_err(|_| for_model("Aufgabenliste nicht lesbar."))?;
                let mut lines: Vec<String> = tasks
                    .iter()
                    .filter(|(_, task)| task.owner == owner)
                    .map(|(id, task)| status_text(*id, task))
                    .collect();
                lines.sort();
                Ok(if lines.is_empty() {
                    "Keine Aufgaben.".to_owned()
                } else {
                    lines.join("\n")
                })
            }
        }
    }
}

/// Eines der fünf Steuer-Tools.
struct Control {
    spec: ToolSpec,
    agents: BackgroundAgents,
    action: &'static str,
}

impl Control {
    fn new(agents: BackgroundAgents, action: &'static str) -> Self {
        let id = json!({"type": "string", "description": "Kennung der Aufgabe aus dem Start"});
        let (description, schema, effect) = match action {
            "start" => (
                format!("Startet eine Aufgabe für den Hintergrund-Agenten {}: {}. Kehrt sofort zurück.", agents.name, agents.description),
                json!({"type": "object", "properties": {"task": {"type": "string", "description": "Vollständige Aufgabe"}}, "required": ["task"]}),
                Effect::External,
            ),
            "check" => (
                "Stand und, wenn fertig, Ergebnis einer Hintergrund-Aufgabe. Mit wait_seconds wartet es bis zu 60 Sekunden.".to_owned(),
                json!({"type": "object", "properties": {"task_id": id, "wait_seconds": {"type": "integer", "minimum": 0, "maximum": 60}}, "required": ["task_id"]}),
                Effect::ReadOnly,
            ),
            "message" => (
                "Schickt einer Hintergrund-Aufgabe eine Nachricht; eine fertige Aufgabe wird damit fortgesetzt.".to_owned(),
                json!({"type": "object", "properties": {"task_id": id, "message": {"type": "string"}}, "required": ["task_id", "message"]}),
                Effect::External,
            ),
            "cancel" => (
                "Bricht eine Hintergrund-Aufgabe ab.".to_owned(),
                json!({"type": "object", "properties": {"task_id": id}, "required": ["task_id"]}),
                Effect::Write,
            ),
            _ => ("Listet die Hintergrund-Aufgaben dieser Sitzung.".to_owned(), json!({"type": "object"}), Effect::ReadOnly),
        };
        let name = format!("{}__{action}", agents.name);
        let mut spec = ToolSpec::new(name, description, schema).with_effect(effect);
        if action == "check" {
            spec = spec.with_timeout(MAX_WAIT + Duration::from_secs(5));
        }
        Self {
            spec,
            agents,
            action,
        }
    }
}

#[async_trait]
impl Tool for Control {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.agents
            .act(self.action, args, cx)
            .await
            .map(ToolOutput::text)
    }
}
