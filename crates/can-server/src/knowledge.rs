//! Das Gedächtnis des Mandanten über die API: auflisten, durchsuchen, ergänzen, löschen.
//!
//! Agenten mit `memory = true` lesen und schreiben dasselbe Gedächtnis über ihre Tools; der
//! Namensraum ist immer der Mandant des Schlüssels.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::App;
use crate::api::{ApiError, require};
use crate::auth::{Principal, Role};

/// Höchstens so viele Treffer pro Suche.
const MAX_LIMIT: usize = 100;

fn internal(cause: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, cause.to_string())
}

#[derive(Deserialize)]
pub struct Search {
    /// Stichworte; ohne sie kommen alle Einträge, neueste zuerst.
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /v1/memories?query=…&limit=…`
pub async fn list(
    State(app): State<Arc<App>>,
    principal: Principal,
    Query(search): Query<Search>,
) -> Result<Response, ApiError> {
    let limit = search.limit.unwrap_or(20).clamp(1, MAX_LIMIT);
    let memories = match search.query.as_deref().map(str::trim) {
        Some(query) if !query.is_empty() => app
            .memory
            .search(&principal.tenant, query, limit)
            .await
            .map_err(internal)?,
        _ => {
            let mut all = app.memory.all(&principal.tenant).await.map_err(internal)?;
            all.reverse();
            all.truncate(limit);
            all
        }
    };
    Ok(Json(json!({"memories": memories})).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewMemory {
    text: String,
}

/// `POST /v1/memories` mit `{"text": …}`; braucht die Rolle Operator.
pub async fn add(
    State(app): State<Arc<App>>,
    principal: Principal,
    Json(body): Json<NewMemory>,
) -> Result<Response, ApiError> {
    require(&principal, Role::Operator)?;
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "`text` ist leer.".to_owned(),
        ));
    }
    let id = app
        .memory
        .add(&principal.tenant, text, app.clock.now())
        .await
        .map_err(internal)?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

/// `DELETE /v1/memories/{id}`; braucht die Rolle Operator.
pub async fn delete(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require(&principal, Role::Operator)?;
    match app
        .memory
        .delete(&principal.tenant, &id)
        .await
        .map_err(internal)?
    {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(ApiError(
            StatusCode::NOT_FOUND,
            "Eintrag nicht gefunden.".to_owned(),
        )),
    }
}
