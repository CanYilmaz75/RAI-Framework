//! Woher der Dienst seine Schlüssel bezieht: Umgebungsvariablen, eingehängte Dateien oder Vault.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// Quelle für Secrets. Die Konfiguration nennt immer nur Namen, nie Werte.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum SecretSource {
    /// Umgebungsvariablen des Prozesses.
    #[default]
    Env,
    /// Eine Datei pro Secret in einem Verzeichnis, wie Kubernetes oder ein Vault-Agent sie einhängen.
    Files { dir: PathBuf },
    /// HashiCorp Vault, KV-Engine Version 2. `token_env` nennt die Variable mit dem Vault-Token.
    Vault {
        addr: String,
        path: String,
        token_env: String,
    },
}

/// Fehler beim Lesen der Secrets.
#[derive(Debug, thiserror::Error)]
#[error("Secrets: {0}")]
pub struct SecretError(pub String);

impl SecretSource {
    /// Liest die genannten Secrets. Was die Quelle nicht kennt, fehlt im Ergebnis; für Dateien
    /// und Vault dienen Umgebungsvariablen als Rückfall.
    pub async fn resolve(&self, names: &[String]) -> Result<HashMap<String, String>, SecretError> {
        let from_env = |name: &String| std::env::var(name).ok().map(|value| (name.clone(), value));
        let mut found: HashMap<String, String> = names.iter().filter_map(from_env).collect();
        match self {
            SecretSource::Env => {}
            SecretSource::Files { dir } => {
                for name in names {
                    // Namen sind schlichte Bezeichner; alles andere könnte aus dem Verzeichnis führen.
                    if !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    {
                        continue;
                    }
                    if let Ok(value) = std::fs::read_to_string(dir.join(name)) {
                        found.insert(name.clone(), value.trim().to_owned());
                    }
                }
            }
            SecretSource::Vault {
                addr,
                path,
                token_env,
            } => {
                let token = std::env::var(token_env)
                    .map_err(|_| SecretError(format!("Variable {token_env} fehlt")))?;
                let url = format!(
                    "{}/v1/{}",
                    addr.trim_end_matches('/'),
                    path.trim_start_matches('/')
                );
                let failed = |error: &dyn std::fmt::Display| {
                    SecretError(format!("Vault nicht lesbar: {error}"))
                };
                let response = reqwest::Client::new()
                    .get(url)
                    .header("x-vault-token", token)
                    .send()
                    .await
                    .map_err(|error| failed(&error))?;
                if !response.status().is_success() {
                    return Err(failed(&format!("HTTP {}", response.status().as_u16())));
                }
                let text = response.text().await.map_err(|error| failed(&error))?;
                let body: serde_json::Value =
                    serde_json::from_str(&text).map_err(|error| failed(&error))?;
                for name in names {
                    if let Some(value) = body
                        .pointer(&format!("/data/data/{name}"))
                        .and_then(|value| value.as_str())
                    {
                        found.insert(name.clone(), value.to_owned());
                    }
                }
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[tokio::test]
    async fn env_source_reads_the_environment() {
        let found = SecretSource::Env
            .resolve(&names(&["PATH", "CAN_GIBT_ES_NICHT"]))
            .await
            .unwrap();
        assert!(found.contains_key("PATH") && !found.contains_key("CAN_GIBT_ES_NICHT"));
    }

    #[tokio::test]
    async fn file_source_reads_one_file_per_secret() {
        let dir = std::env::temp_dir().join(format!("can-secrets-{}", can_core::SessionId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MODELL_KEY"), "aus-datei\n").unwrap();
        let source = SecretSource::Files { dir: dir.clone() };
        let found = source
            .resolve(&names(&["MODELL_KEY", "FEHLT", "../etc/passwd"]))
            .await
            .unwrap();
        assert_eq!(
            found.get("MODELL_KEY").map(String::as_str),
            Some("aus-datei")
        );
        assert_eq!(found.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn vault_source_reads_kv2_and_reports_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let (head_tx, head_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut head_tx = Some(head_tx);
            for status in ["200 OK", "403 Forbidden"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                if let Some(tx) = head_tx.take() {
                    let _ = tx.send(String::from_utf8_lossy(&buffer[..read]).to_lowercase());
                }
                let body = r#"{"data":{"data":{"MODELL_KEY":"aus-vault","ANDERES":"x"}}}"#;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        // PATH dient als Token: so braucht der Test keine eigene Umgebungsvariable.
        let source = SecretSource::Vault {
            addr,
            path: "secret/data/can".into(),
            token_env: "PATH".into(),
        };
        let found = source
            .resolve(&names(&["MODELL_KEY", "FEHLT"]))
            .await
            .unwrap();
        assert_eq!(
            found.get("MODELL_KEY").map(String::as_str),
            Some("aus-vault")
        );
        assert!(!found.contains_key("FEHLT"));
        let head = head_rx.await.unwrap();
        assert!(head.starts_with("get /v1/secret/data/can") && head.contains("x-vault-token:"));
        assert!(
            source
                .resolve(&names(&["MODELL_KEY"]))
                .await
                .unwrap_err()
                .to_string()
                .contains("403")
        );
        let missing = SecretSource::Vault {
            addr: "http://x".into(),
            path: "p".into(),
            token_env: "CAN_GIBT_ES_NICHT".into(),
        };
        assert!(missing.resolve(&[]).await.is_err());
    }

    #[test]
    fn config_forms_parse() {
        let vault: SecretSource = serde_json::from_value(
            serde_json::json!({"kind": "vault", "addr": "http://v", "path": "p", "token_env": "T"}),
        )
        .unwrap();
        assert!(matches!(vault, SecretSource::Vault { .. }));
        assert_eq!(
            serde_json::from_value::<SecretSource>(serde_json::json!({"kind": "env"})).unwrap(),
            SecretSource::Env
        );
        assert!(
            serde_json::from_value::<SecretSource>(serde_json::json!({"kind": "files"})).is_err()
        );
    }
}
