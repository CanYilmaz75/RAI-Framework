//! Event-Strom einer Sitzung über Server-Sent Events.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use can_core::{Envelope, Event, SessionId};
use futures::Stream;
use tokio::sync::{broadcast, mpsc};

use crate::App;
use crate::api::{After, ApiError, session_of};
use crate::auth::Principal;

fn sse_event(envelope: &Envelope) -> SseEvent {
    let event = SseEvent::default()
        .event(envelope.event.kind())
        .data(serde_json::to_string(envelope).unwrap_or_default());
    // Nur dauerhafte Events tragen eine Kennung: an ihnen setzt ein Client wieder auf.
    if envelope.event.is_durable() {
        event.id(envelope.seq.to_string())
    } else {
        event
    }
}

/// Event-Strom der Sitzung: erst das Gespeicherte nach `after` (oder `Last-Event-ID`), dann
/// live, bis der Lauf endet.
pub async fn events(
    State(app): State<Arc<App>>,
    principal: Principal,
    Path(id): Path<SessionId>,
    Query(query): Query<After>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    session_of(&app, &principal, id).await?;
    let header = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let mut last: Option<u64> = query.after.or(header);
    let (tx, rx) = mpsc::channel::<SseEvent>(64);
    tokio::spawn(async move {
        // Erst abonnieren, dann den Store lesen: so entsteht keine Lücke dazwischen.
        let mut live = app.runs.live(id).map(|live| live.subscribe());
        loop {
            let stored = app.runs.store.load(id).await.unwrap_or_default();
            for envelope in &stored {
                if last.is_some_and(|last| envelope.seq <= last) {
                    continue;
                }
                if tx.send(sse_event(envelope)).await.is_err() {
                    return;
                }
                last = Some(envelope.seq);
            }
            let Some(receiver) = live.as_mut() else {
                return;
            };
            loop {
                match receiver.recv().await {
                    Ok(envelope) => {
                        let durable = envelope.event.is_durable();
                        if durable && last.is_some_and(|last| envelope.seq <= last) {
                            continue;
                        }
                        if tx.send(sse_event(&envelope)).await.is_err() {
                            return;
                        }
                        if durable {
                            last = Some(envelope.seq);
                        }
                        if matches!(envelope.event, Event::RunFinished { .. }) {
                            return;
                        }
                    }
                    // Zu langsam gelesen: das Verpasste kommt aus dem Store.
                    Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(broadcast::error::RecvError::Closed) => {
                        live = None;
                        break;
                    }
                }
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
