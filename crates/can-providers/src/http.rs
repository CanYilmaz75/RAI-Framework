//! Gemeinsamer HTTP-Teil der Anbieter: Anfrage senden, Fehlerstatus zuordnen, Body als Strom.

use std::time::Duration;

use can_core::ProviderError;
use futures::{Stream, StreamExt};

pub(crate) fn network(error: reqwest::Error) -> ProviderError {
    ProviderError::Network(Box::new(error))
}

/// Ordnet eine HTTP-Fehlerantwort einem `ProviderError` zu.
pub(crate) fn error_for_status(
    status: u16,
    retry_after: Option<&str>,
    body: &str,
) -> ProviderError {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(500).collect());
    let lower = message.to_lowercase();
    match status {
        401 | 403 => ProviderError::Auth,
        429 => ProviderError::RateLimited {
            retry_after: retry_after
                .and_then(|v| v.trim().parse().ok())
                .map(Duration::from_secs),
        },
        400 | 413 if lower.contains("context") || lower.contains("too long") => {
            ProviderError::ContextTooLong { limit: 0 }
        }
        500..=599 => ProviderError::Overloaded,
        _ => ProviderError::InvalidRequest(format!("HTTP {status}: {message}")),
    }
}

/// Sendet die Anfrage und liefert bei Erfolg den Body als Strom von Byte-Blöcken.
pub(crate) async fn send(
    builder: reqwest::RequestBuilder,
) -> Result<
    impl Stream<Item = Result<Vec<u8>, ProviderError>> + Send + Unpin + 'static,
    ProviderError,
> {
    let response = builder.send().await.map_err(network)?;
    let status = response.status();
    if !status.is_success() {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response.text().await.unwrap_or_default();
        return Err(error_for_status(
            status.as_u16(),
            retry_after.as_deref(),
            &text,
        ));
    }
    Ok(Box::pin(response.bytes_stream().map(|item| {
        item.map(|bytes| bytes.to_vec()).map_err(network)
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_errors_map_to_provider_errors() {
        assert!(matches!(
            error_for_status(401, None, ""),
            ProviderError::Auth
        ));
        assert!(matches!(
            error_for_status(429, Some("7"), ""),
            ProviderError::RateLimited { retry_after: Some(d) } if d.as_secs() == 7
        ));
        assert!(matches!(
            error_for_status(503, None, ""),
            ProviderError::Overloaded
        ));
        assert!(matches!(
            error_for_status(529, None, ""),
            ProviderError::Overloaded
        ));
        let body = r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#;
        assert!(matches!(
            error_for_status(400, None, body),
            ProviderError::ContextTooLong { .. }
        ));
        let long = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens"}}"#;
        assert!(matches!(
            error_for_status(400, None, long),
            ProviderError::ContextTooLong { .. }
        ));
        let other = error_for_status(404, None, r#"{"error":{"message":"model not found"}}"#);
        assert_eq!(
            other.to_string(),
            "ungültige Anfrage: HTTP 404: model not found"
        );
    }
}
