//! Tool `http_fetch`: holt eine Seite per GET, aber nur von freigegebenen, öffentlichen Hosts.
//!
//! Private und Link-Local-Adressen sind gesperrt, auch nach DNS-Auflösung und nach jeder
//! Weiterleitung. Die geprüfte Adresse wird für die Verbindung festgenagelt, damit zwischen
//! Prüfung und Zugriff keine andere Auflösung liegen kann.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use can_core::{Effect, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec, rules::glob};
use futures::StreamExt;
use reqwest::Url;
use serde_json::{Value, json};

const MAX_REDIRECTS: usize = 5;

/// HTTP-GET mit Host-Freigabeliste.
pub struct HttpFetch {
    spec: ToolSpec,
    allowed_hosts: Vec<String>,
    allow_private: bool,
    max_bytes: usize,
}

fn for_model(message: impl Into<String>) -> ToolError {
    ToolError::ForModel(message.into())
}

/// Ist die Adresse für das Tool gesperrt: Loopback, private Netze, Link-Local und Ähnliches?
pub fn is_blocked(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (a == 100 && (64..128).contains(&b))
                || a == 0
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_blocked(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || first & 0xfe00 == 0xfc00
                    || first & 0xffc0 == 0xfe80
            }
        },
    }
}

impl HttpFetch {
    /// Tool, das nur Hosts abruft, die auf eines der Muster passen (`*` als Platzhalter).
    pub fn new(allowed_hosts: Vec<String>) -> Self {
        let schema = json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "Adresse mit http:// oder https://"}},
            "required": ["url"],
            "additionalProperties": false
        });
        let spec = ToolSpec::new(
            "http_fetch",
            "Ruft eine Webseite per GET ab und liefert Status und Text.",
            schema,
        )
        .with_effect(Effect::External)
        .with_idempotent(true)
        .with_timeout(Duration::from_secs(30));
        Self {
            spec,
            allowed_hosts,
            allow_private: false,
            max_bytes: 1024 * 1024,
        }
    }

    /// Hebt die Sperre für private Adressen auf. Nur für Tests und bewusst interne Dienste.
    pub fn allow_private_networks(mut self, allow: bool) -> Self {
        self.allow_private = allow;
        self
    }

    /// Prüft Schema, Freigabeliste und aufgelöste Adressen; liefert die Adresse für die Verbindung.
    async fn check(&self, url: &Url) -> Result<(String, SocketAddr), ToolError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(for_model(format!("Nur http und https sind erlaubt: {url}")));
        }
        let host = url
            .host_str()
            .ok_or_else(|| for_model(format!("Adresse ohne Host: {url}")))?;
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        if !self
            .allowed_hosts
            .iter()
            .any(|pattern| glob(pattern, &host))
        {
            return Err(for_model(format!(
                "Der Host {host} steht nicht auf der Freigabeliste."
            )));
        }
        let port = url.port_or_known_default().unwrap_or(80);
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|error| for_model(format!("Host {host} nicht auflösbar: {error}")))?
            .collect();
        if !self.allow_private && addresses.iter().any(|address| is_blocked(address.ip())) {
            return Err(for_model(format!(
                "Der Host {host} zeigt auf eine private oder lokale Adresse."
            )));
        }
        let address = addresses
            .first()
            .copied()
            .ok_or_else(|| for_model(format!("Host {host} hat keine Adresse.")))?;
        Ok((host, address))
    }

    async fn fetch(&self, start: &str) -> Result<String, ToolError> {
        let mut url =
            Url::parse(start).map_err(|error| for_model(format!("Ungültige Adresse: {error}")))?;
        for _ in 0..=MAX_REDIRECTS {
            let (host, address) = self.check(&url).await?;
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .resolve(&host, address)
                .build()
                .map_err(|error| ToolError::Fatal(Box::new(error)))?;
            let response = client
                .get(url.clone())
                .send()
                .await
                .map_err(|error| for_model(format!("Abruf fehlgeschlagen: {error}")))?;
            let status = response.status();
            if status.is_redirection()
                && let Some(location) = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
            {
                url = url
                    .join(location)
                    .map_err(|error| for_model(format!("Ungültige Weiterleitung: {error}")))?;
                continue;
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|error| for_model(format!("Abruf abgebrochen: {error}")))?;
                body.extend_from_slice(&chunk);
                if body.len() > self.max_bytes {
                    body.truncate(self.max_bytes);
                    break;
                }
            }
            return Ok(format!(
                "HTTP {}\n\n{}",
                status.as_u16(),
                String::from_utf8_lossy(&body)
            ));
        }
        Err(for_model(format!(
            "Mehr als {MAX_REDIRECTS} Weiterleitungen."
        )))
    }
}

#[async_trait]
impl Tool for HttpFetch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| for_model("Argument `url` fehlt."))?;
        tokio::select! {
            result = self.fetch(url) => result.map(ToolOutput::text),
            _ = cx.cancel.cancelled() => Err(for_model("Abgebrochen.")),
        }
    }
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
