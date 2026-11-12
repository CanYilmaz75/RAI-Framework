//! Die HTTP-Schnittstelle: Sitzungen, Nachrichten, Event-Strom, Freigaben.

use std::sync::Arc;

use axum::Json;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};
use can_core::{CallId, Decision, Envelope, Message, SessionId, Timestamp};
use can_runtime::RunError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::App;
use crate::auth::{Principal, Role};
use crate::meta::{SessionMeta, SessionStatus};
use crate::runs::MessageMode;

/// Fehlerantwort: Status und Meldung als JSON.
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError(status, message.into())
}

fn internal(cause: impl std::fmt::Display) -> ApiError {
    error(StatusCode::INTERNAL_SERVER_ERROR, cause.to_string())
}

impl FromRequestParts<Arc<App>> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        app.directory.authenticate(header).ok_or_else(|| {
            error(
                StatusCode::UNAUTHORIZED,
                "Kein gültiger API-Schlüssel im Header Authorization.",
            )
        })
    }
}

pub(crate) fn require(principal: &Principal, role: Role) -> Result<(), ApiError> {
    if principal.role >= role {
        return Ok(());
    }
    Err(error(
        StatusCode::FORBIDDEN,
        format!("Dafür ist die Rolle {role:?} nötig."),
    ))
}

/// Die Sitzung, sofern sie dem Mandanten des Aufrufers gehört. Fremde Sitzungen sehen aus wie
/// nicht vorhandene.
pub(crate) async fn session_of(
    app: &App,
    principal: &Principal,
    id: SessionId,
) -> Result<SessionMeta, ApiError> {
    let meta = app.runs.directory.get(id).await.map_err(internal)?;
    meta.filter(|meta| meta.tenant == principal.tenant)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Sitzung nicht gefunden."))
}

/// Prüft die Kontingente des Mandanten. `new_run` zählt einen zusätzlichen gleichzeitigen Lauf.
fn check_quota(app: &App, principal: &Principal, new_run: bool) -> Result<(), ApiError> {
    let quota = app.directory.quota(&principal.tenant);
    if new_run && app.runs.running_for(&principal.tenant) >= quota.max_concurrent_runs {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Zu viele gleichzeitige Läufe für diesen Mandanten.",
        ));
    }
    if quota.max_tokens > 0 && app.runs.metrics.tokens_used(&principal.tenant) >= quota.max_tokens {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Das Token-Budget dieses Mandanten ist aufgebraucht.",
        ));
    }
    if quota.max_cost_cents > 0
        && app.runs.metrics.cost_cents(&principal.tenant) >= quota.max_cost_cents
    {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Das Kostenbudget dieses Mandanten ist aufgebraucht.",
        ));
    }
    Ok(())
}

fn run_error(cause: RunError) -> ApiError {
    match cause {
        RunError::Interrupted => error(
            StatusCode::CONFLICT,
            "In dieser Sitzung läuft noch ein Lauf.",
        ),
        RunError::NothingToResume => error(
            StatusCode::CONFLICT,
            "In dieser Sitzung gibt es nichts fortzusetzen.",
        ),
        other => internal(other),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    agent: String,
    input: String,
}

pub async fn create_session(
    State(app): State<Arc<App>>,
    principal: Principal,
    headers: HeaderMap,
    Json(body): Json<CreateSession>,
) -> Result<Response, ApiError> {
    require(&principal, Role::Operator)?;
    if !app.runs.agents.contains_key(&body.agent) {
        return Err(error(
            StatusCode::NOT_FOUND,
            format!("Unbekannter Agent: {}", body.agent),
        ));
    }
    check_quota(&app, &principal, true)?;
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let wanted = SessionMeta {
        id: SessionId::new(),
        tenant: principal.tenant.clone(),
        user: principal.user.clone(),
        agent: body.agent,
        created_at: Timestamp(app.clock.now().0),
        status: SessionStatus::Running,
        idempotency_key,
    };
    let meta = app
        .runs
        .directory
        .create(wanted.clone())
        .await
        .map_err(internal)?;
    if meta.id != wanted.id {
        // Derselbe Idempotenz-Schlüssel: die erste Sitzung gilt, es startet kein zweiter Lauf.
        return Ok((
            StatusCode::OK,
            Json(json!({"session_id": meta.id, "created": false})),
        )
            .into_response());
    }
    app.runs
        .start(&meta, Message::user(body.input))
        .await
        .map_err(run_error)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"session_id": meta.id, "created": true})),
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostMessage {
    input: String,
    /// Was geschieht, wenn in der Sitzung schon ein Lauf unterwegs ist.
    #[serde(default)]
    mode: MessageMode,
}

pub async fn post_message(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
    Json(body): Json<PostMessage>,
) -> Result<Response, ApiError> {
    require(&principal, Role::Operator)?;
    let meta = session_of(&app, &principal, id).await?;
    // Ein neuer gleichzeitiger Lauf entsteht nur, wenn in der Sitzung gerade keiner läuft.
    check_quota(&app, &principal, app.runs.live(id).is_none())?;
    let delivery = app
        .runs
        .post(&meta, Message::user(body.input), body.mode)
        .await
        .map_err(run_error)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"delivery": delivery, "queued": app.runs.queued(id)})),
    )
        .into_response())
}

pub async fn list_sessions(
    State(app): State<Arc<App>>,
    principal: Principal,
) -> Result<Json<Value>, ApiError> {
    let sessions = app
        .runs
        .directory
        .list(&principal.tenant)
        .await
        .map_err(internal)?;
    Ok(Json(json!({"sessions": sessions})))
}

pub async fn get_session(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
) -> Result<Json<Value>, ApiError> {
    let meta = session_of(&app, &principal, id).await?;
    let approvals: Vec<_> = app
        .runs
        .approvals(Some(&principal.tenant))
        .into_iter()
        .filter(|approval| approval.session_id == id)
        .collect();
    // Der Plan des Agenten ergibt sich aus dem Log.
    let plan = match app.runs.agents.get(&meta.agent) {
        Some(agent) => {
            let log = app.runs.store.load(id).await.map_err(internal)?;
            can_core::RunState::fold(agent.config().clone(), &log).plan
        }
        None => Vec::new(),
    };
    Ok(Json(json!({
        "session": meta,
        "running_here": app.runs.live(id).is_some(),
        "pending_approvals": approvals,
        "plan": plan,
    })))
}

pub async fn cancel(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, ApiError> {
    require(&principal, Role::Operator)?;
    session_of(&app, &principal, id).await?;
    if app.runs.cancel(id) {
        return Ok(StatusCode::ACCEPTED);
    }
    Err(error(
        StatusCode::CONFLICT,
        "In dieser Sitzung läuft hier kein Lauf.",
    ))
}

pub async fn resume(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, ApiError> {
    require(&principal, Role::Operator)?;
    let meta = session_of(&app, &principal, id).await?;
    if app.runs.live(id).is_some() {
        return Err(error(
            StatusCode::CONFLICT,
            "Der Lauf ist bereits unterwegs.",
        ));
    }
    app.runs.resume(&meta).await.map_err(run_error)?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn list_approvals(State(app): State<Arc<App>>, principal: Principal) -> Json<Value> {
    Json(json!({"approvals": app.runs.approvals(Some(&principal.tenant))}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolve {
    decision: Decision,
    /// Nur mit `allow`: Argumente, mit denen das Tool statt denen des Modells läuft.
    input: Option<Value>,
}

pub async fn resolve_approval(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path((id, call_id)): Path<(SessionId, CallId)>,
    Json(body): Json<Resolve>,
) -> Result<StatusCode, ApiError> {
    require(&principal, Role::Operator)?;
    let meta = session_of(&app, &principal, id).await?;
    if body.decision == Decision::Ask {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Erlaubt sind allow und deny.",
        ));
    }
    if let Some(input) = body.input {
        if body.decision != Decision::Allow {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Geänderte Argumente gibt es nur mit allow.",
            ));
        }
        return match app.runs.approve_with(&meta.agent, id, call_id, input).await {
            Ok(true) => Ok(StatusCode::NO_CONTENT),
            Ok(false) => Err(error(
                StatusCode::NOT_FOUND,
                "Diese Rückfrage ist nicht (mehr) offen.",
            )),
            Err(problem) => Err(error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Argumente passen nicht: {problem}"),
            )),
        };
    }
    if app.runs.resolve(id, call_id, body.decision).await {
        return Ok(StatusCode::NO_CONTENT);
    }
    Err(error(
        StatusCode::NOT_FOUND,
        "Diese Rückfrage ist nicht (mehr) offen.",
    ))
}

pub async fn list_agents(State(app): State<Arc<App>>, _principal: Principal) -> Json<Value> {
    let mut names: Vec<&String> = app.runs.agents.keys().collect();
    names.sort();
    Json(json!({"agents": names}))
}

#[derive(Deserialize)]
pub struct After {
    pub after: Option<u64>,
}

/// Gespeicherte Events der Sitzung nach Position `after`, als JSON.
pub async fn log(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
    Query(query): Query<After>,
) -> Result<Json<Value>, ApiError> {
    session_of(&app, &principal, id).await?;
    let events = app.runs.store.load(id).await.map_err(internal)?;
    let events: Vec<Envelope> = events
        .into_iter()
        .filter(|e| query.after.is_none_or(|after| e.seq > after))
        .collect();
    Ok(Json(json!({"events": events})))
}

/// Löscht eine Sitzung samt Log, etwa auf Wunsch des Nutzers oder nach Ablauf einer Frist.
pub async fn delete_session(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, ApiError> {
    require(&principal, Role::Admin)?;
    session_of(&app, &principal, id).await?;
    if app.runs.live(id).is_some() {
        return Err(error(
            StatusCode::CONFLICT,
            "Der Lauf ist noch unterwegs; zuerst abbrechen.",
        ));
    }
    app.runs.store.delete(id).await.map_err(internal)?;
    app.runs.directory.delete(id).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Prüft die Hash-Kette der Sitzung: wurde das Log nach dem Schreiben verändert?
pub async fn verify(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, Role::Admin)?;
    session_of(&app, &principal, id).await?;
    let broken = app.runs.store.verify(id).await.map_err(internal)?;
    Ok(Json(
        json!({"intact": broken.is_none(), "first_broken_seq": broken}),
    ))
}
