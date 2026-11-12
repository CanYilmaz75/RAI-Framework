//! HTTP-Dienst um die Can-Laufzeit: Sitzungen, Event-Strom, Freigaben, Mandanten und Rollen.

pub mod api;
pub mod auth;
pub mod branch;
mod build;
pub mod config;
pub mod cron;
pub mod knowledge;
pub mod meta;
pub mod metrics;
pub mod pgmeta;
mod queue;
pub mod runs;
pub mod schedule;
pub mod secrets;
pub mod stream;

use std::sync::Arc;

use axum::Router;
use axum::response::Html;
use axum::routing::{delete, get, post};
use can_core::{Clock, MemoryBank};

pub use auth::{Directory, Principal, Quota, Role};
pub use config::ServerConfig;
pub use meta::{MemoryDirectory, SessionDirectory, SessionMeta, SessionStatus};
pub use pgmeta::PostgresDirectory;
pub use runs::RunManager;
pub use schedule::Schedules;

/// Alles, was die Handler brauchen.
pub struct App {
    pub runs: Arc<RunManager>,
    pub directory: Directory,
    pub clock: Arc<dyn Clock>,
    /// Gedächtnis aller Mandanten; jeder sieht nur seinen Namensraum.
    pub memory: Arc<dyn MemoryBank>,
    /// Geplante Läufe; `schedule::run` löst sie aus.
    pub schedules: Arc<Schedules>,
}

/// Die Routen des Dienstes. `/healthz`, `/metrics` und die Oberfläche brauchen keinen Schlüssel;
/// alles unter `/v1` schon.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("ui.html")) }))
        .route(
            "/openapi.json",
            get(|| async {
                (
                    [("content-type", "application/json")],
                    include_str!("openapi.json"),
                )
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics::handler))
        .route("/v1/agents", get(api::list_agents))
        .route(
            "/v1/sessions",
            post(api::create_session).get(api::list_sessions),
        )
        .route(
            "/v1/sessions/{id}",
            get(api::get_session).delete(api::delete_session),
        )
        .route("/v1/sessions/{id}/messages", post(api::post_message))
        .route("/v1/sessions/{id}/events", get(stream::events))
        .route("/v1/sessions/{id}/log", get(api::log))
        .route("/v1/sessions/{id}/verify", get(api::verify))
        .route("/v1/sessions/{id}/cancel", post(api::cancel))
        .route("/v1/sessions/{id}/resume", post(api::resume))
        .route("/v1/sessions/{id}/fork", post(branch::fork))
        .route(
            "/v1/sessions/{id}/approvals/{call_id}",
            post(api::resolve_approval),
        )
        .route("/v1/approvals", get(api::list_approvals))
        .route("/v1/schedules", get(schedule::list))
        .route("/v1/memories", get(knowledge::list).post(knowledge::add))
        .route("/v1/memories/{id}", delete(knowledge::delete))
        .with_state(app)
}
