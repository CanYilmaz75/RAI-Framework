//! Verzweigen einer Sitzung über die API.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use can_core::{SessionId, Timestamp};
use can_runtime::RunError;
use serde::Deserialize;
use serde_json::json;

use crate::App;
use crate::api::{ApiError, session_of};
use crate::auth::{Principal, Role};
use crate::meta::{SessionMeta, SessionStatus};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fork {
    /// Letzte Position, die der Zweig übernimmt.
    at: u64,
}

/// Verzweigt die Sitzung: eine neue übernimmt den Verlauf bis zur Position `at`.
pub async fn fork(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
    Json(body): Json<Fork>,
) -> Result<Response, ApiError> {
    if principal.role < Role::Operator {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "Dafür ist die Rolle Operator nötig.".to_owned(),
        ));
    }
    let source = session_of(&app, &principal, id).await?;
    let internal = |cause: &dyn std::fmt::Display| {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, cause.to_string())
    };
    let agent = app.runs.agents.get(&source.agent);
    let agent = agent.ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "Agent nicht mehr vorhanden.".to_owned(),
        )
    })?;
    let branch = agent.fork(id, body.at).await.map_err(|cause| match cause {
        RunError::NothingToFork => ApiError(
            StatusCode::BAD_REQUEST,
            "An dieser Position gibt es nichts zu verzweigen.".to_owned(),
        ),
        other => internal(&other),
    })?;
    let meta = SessionMeta {
        id: branch,
        tenant: principal.tenant.clone(),
        user: principal.user.clone(),
        agent: source.agent,
        created_at: Timestamp(app.clock.now().0),
        status: SessionStatus::Idle,
        idempotency_key: None,
    };
    app.runs
        .directory
        .create(meta)
        .await
        .map_err(|cause| internal(&cause))?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"session_id": branch, "forked_from": id, "at": body.at})),
    )
        .into_response())
}
