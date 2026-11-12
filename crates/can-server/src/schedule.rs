//! Geplante Läufe: Zeitpläne aus der Konfiguration starten Läufe nach einem Cron-Ausdruck.
//!
//! Jede Auslösung legt eine neue Sitzung an. Ihr Idempotenz-Schlüssel `zeitplan:<name>:<minute>`
//! sorgt dafür, dass bei mehreren Workern nur einer startet. Verpasste Zeitpunkte, etwa während
//! eines Neustarts, werden nicht nachgeholt.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use can_core::{Message, SessionId, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::App;
use crate::auth::Principal;
use crate::config::{ConfigError, ServerConfig, invalid};
use crate::cron::Cron;
use crate::meta::{SessionMeta, SessionStatus};

/// Millisekunden pro Minute.
const MINUTE: u64 = 60_000;

/// Ein Zeitplan aus `[[schedules]]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    pub name: String,
    /// Fünf Felder in UTC, etwa `"0 7 * * 1-5"`, oder `@daily`.
    pub cron: Cron,
    pub agent: String,
    /// Mandant, dem die Sitzungen gehören; er trägt Kontingente und Kosten.
    pub tenant: String,
    /// Nutzer, der in den Sitzungen steht.
    #[serde(default = "scheduler_user")]
    pub user: String,
    /// Die Nachricht, mit der jeder Lauf beginnt.
    pub input: String,
}

fn scheduler_user() -> String {
    "zeitplan".to_owned()
}

/// Prüft die Zeitpläne gegen Agenten und Mandanten der Konfiguration.
pub(crate) fn validate(config: &ServerConfig) -> Result<(), ConfigError> {
    let mut names = std::collections::HashSet::new();
    for schedule in &config.schedules {
        let problem = if !names.insert(&schedule.name) {
            "doppelt"
        } else if !config
            .agents
            .iter()
            .any(|agent| agent.name == schedule.agent)
        {
            "nennt einen unbekannten Agenten"
        } else if !config
            .tenants
            .iter()
            .any(|tenant| tenant.name == schedule.tenant)
        {
            "nennt einen unbekannten Mandanten"
        } else if schedule.input.trim().is_empty() {
            "hat keine Eingabe"
        } else {
            continue;
        };
        return Err(invalid(format!("Zeitplan {} {problem}", schedule.name)));
    }
    Ok(())
}

/// Ergebnis der letzten Auslösung in diesem Prozess.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Fired {
    pub at: Timestamp,
    /// `started`, `elsewhere` (ein anderer Worker war schneller), `skipped` oder `failed`.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Die Zeitpläne des Dienstes samt letzter Auslösung.
#[derive(Default)]
pub struct Schedules {
    list: Vec<ScheduleConfig>,
    last: Mutex<HashMap<String, Fired>>,
}

impl Schedules {
    pub fn new(list: Vec<ScheduleConfig>) -> Self {
        Self {
            list,
            last: Mutex::default(),
        }
    }

    /// Die konfigurierten Zeitpläne.
    pub fn list(&self) -> &[ScheduleConfig] {
        &self.list
    }

    /// Letzte Auslösung eines Zeitplans in diesem Prozess.
    pub fn last(&self, name: &str) -> Option<Fired> {
        self.last.lock().ok()?.get(name).cloned()
    }

    fn record(&self, name: &str, fired: Fired) {
        if let Ok(mut last) = self.last.lock() {
            last.insert(name.to_owned(), fired);
        }
    }
}

/// Startet alle Zeitpläne, die auf diese Minute (seit 1970, UTC) passen. Liefert, wie viele
/// Läufe dieser Prozess gestartet hat.
pub async fn fire_due(app: &Arc<App>, minute: u64) -> usize {
    let mut started = 0;
    for schedule in app
        .schedules
        .list()
        .iter()
        .filter(|s| s.cron.matches(minute))
    {
        // Diese Minute hat dieser Prozess schon behandelt.
        let at = Timestamp(minute * MINUTE);
        if app
            .schedules
            .last(&schedule.name)
            .is_some_and(|last| last.at == at)
        {
            continue;
        }
        let fired = fire(app, schedule, minute).await;
        match fired.outcome {
            "started" => started += 1,
            "elsewhere" => {}
            outcome => {
                tracing::warn!(schedule = %schedule.name, outcome, reason = ?fired.reason, "Zeitplan nicht gestartet")
            }
        }
        app.schedules.record(&schedule.name, fired);
    }
    started
}

async fn fire(app: &Arc<App>, schedule: &ScheduleConfig, minute: u64) -> Fired {
    let at = Timestamp(minute * MINUTE);
    let result = |outcome, session_id, reason: Option<String>| Fired {
        at,
        outcome,
        session_id,
        reason,
    };
    let quota = app.directory.quota(&schedule.tenant);
    if app.runs.running_for(&schedule.tenant) >= quota.max_concurrent_runs {
        let reason = "Mandant hat schon die erlaubte Zahl gleichzeitiger Läufe".to_owned();
        return result("skipped", None, Some(reason));
    }
    let wanted = SessionMeta {
        id: SessionId::new(),
        tenant: schedule.tenant.clone(),
        user: schedule.user.clone(),
        agent: schedule.agent.clone(),
        created_at: Timestamp(app.clock.now().0),
        status: SessionStatus::Running,
        idempotency_key: Some(format!("zeitplan:{}:{minute}", schedule.name)),
    };
    let meta = match app.runs.directory.create(wanted.clone()).await {
        Ok(meta) => meta,
        Err(error) => return result("failed", None, Some(error.to_string())),
    };
    if meta.id != wanted.id {
        return result("elsewhere", Some(meta.id), None);
    }
    match app.runs.start(&meta, Message::user(&schedule.input)).await {
        Ok(()) => result("started", Some(meta.id), None),
        Err(error) => result("failed", Some(meta.id), Some(error.to_string())),
    }
}

/// Prüft jede Minute die Zeitpläne. Läuft, bis der Prozess endet.
pub async fn run(app: Arc<App>) {
    if app.schedules.list().is_empty() {
        return;
    }
    let mut last = None;
    loop {
        let now = app.clock.now().0;
        let minute = now / MINUTE;
        if last != Some(minute) {
            fire_due(&app, minute).await;
            last = Some(minute);
        }
        let until_next = MINUTE - now % MINUTE;
        app.clock
            .sleep(std::time::Duration::from_millis(until_next))
            .await;
    }
}

/// `GET /v1/schedules`: die Zeitpläne des eigenen Mandanten mit nächster Ausführung.
pub async fn list(State(app): State<Arc<App>>, principal: Principal) -> Json<Value> {
    let minute = app.clock.now().0 / MINUTE;
    let schedules: Vec<Value> = app
        .schedules
        .list()
        .iter()
        .filter(|schedule| schedule.tenant == principal.tenant)
        .map(|schedule| {
            json!({
                "name": schedule.name,
                "cron": schedule.cron.to_string(),
                "agent": schedule.agent,
                "next_run": schedule.cron.next_after(minute).map(|m| Timestamp(m * MINUTE)),
                "last": app.schedules.last(&schedule.name),
            })
        })
        .collect();
    Json(json!({"schedules": schedules}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_are_checked_against_agents_and_tenants() {
        let base = r#"
[[agents]]
name = "helfer"
model = "m"

[[tenants]]
name = "acme"

[[schedules]]
name = "bericht"
cron = "30 6 * * 1-5"
agent = "helfer"
tenant = "acme"
input = "Fasse zusammen."
"#;
        let config = ServerConfig::parse(base).unwrap();
        assert_eq!(config.schedules[0].cron.to_string(), "30 6 * * 1-5");
        assert_eq!(config.schedules[0].user, "zeitplan");
        for (from, to, wanted) in [
            (
                "agent = \"helfer\"\ntenant",
                "agent = \"fehlt\"\ntenant",
                "unbekannten Agenten",
            ),
            (
                "tenant = \"acme\"\ninput",
                "tenant = \"globex\"\ninput",
                "unbekannten Mandanten",
            ),
            ("30 6 * * 1-5", "61 6 * * *", "Cron"),
            ("Fasse zusammen.", " ", "keine Eingabe"),
        ] {
            let error = ServerConfig::parse(&base.replace(from, to)).err().unwrap();
            assert!(error.to_string().contains(wanted), "{error}");
        }
    }
}
