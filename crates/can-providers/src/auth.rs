//! Zugangstoken für Anbieter, deren Token ablaufen (etwa Google Cloud): fest vorgegeben oder
//! von einem Programm geholt und eine Weile zwischengespeichert.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use can_core::{ProviderError, Secret};

/// So lange gilt ein von einem Programm geholtes Token, bevor es neu geholt wird. Google-Token
/// gelten eine Stunde.
const COMMAND_TOKEN_TTL: Duration = Duration::from_secs(45 * 60);

/// Woher das Token kommt.
pub enum TokenSource {
    /// Ein festes Token.
    Static(Secret<String>),
    /// Ein Programm, das das Token auf die Standardausgabe schreibt, etwa
    /// `gcloud auth print-access-token`.
    Command {
        program: String,
        args: Vec<String>,
        cache: Mutex<Option<(Secret<String>, Instant)>>,
    },
}

impl TokenSource {
    /// Token aus einem Programm.
    pub fn command(program: impl Into<String>, args: Vec<String>) -> Self {
        Self::Command {
            program: program.into(),
            args,
            cache: Mutex::new(None),
        }
    }

    /// Das aktuelle Token.
    pub async fn token(&self) -> Result<Secret<String>, ProviderError> {
        let Self::Command {
            program,
            args,
            cache,
        } = self
        else {
            let Self::Static(token) = self else {
                unreachable!()
            };
            return Ok(token.clone());
        };
        if let Ok(cached) = cache.lock()
            && let Some((token, fetched)) = cached.as_ref()
            && fetched.elapsed() < COMMAND_TOKEN_TTL
        {
            return Ok(token.clone());
        }
        let (program, args) = (program.clone(), args.clone());
        let failed = |message: String| {
            ProviderError::InvalidRequest(format!("Token nicht abrufbar: {message}"))
        };
        let output =
            std::thread::spawn(move || std::process::Command::new(&program).args(&args).output())
                .join()
                .map_err(|_| failed("Thread abgebrochen".into()))?
                .map_err(|error| failed(error.to_string()))?;
        if !output.status.success() {
            return Err(failed(
                String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .chars()
                    .take(200)
                    .collect(),
            ));
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if token.is_empty() {
            return Err(failed("das Programm lieferte kein Token".into()));
        }
        let token = Secret::new(token);
        if let Ok(mut cached) = cache.lock() {
            *cached = Some((token.clone(), Instant::now()));
        }
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_and_command_tokens() {
        let fixed = TokenSource::Static(Secret::new("fest".into()));
        assert_eq!(fixed.token().await.unwrap().expose(), "fest");
        let command = TokenSource::command("echo", vec!["  token-123  ".into()]);
        assert_eq!(command.token().await.unwrap().expose(), "token-123");
        assert_eq!(
            command.token().await.unwrap().expose(),
            "token-123",
            "aus dem Zwischenspeicher"
        );
        let failing = TokenSource::command("false", vec![]);
        assert!(
            matches!(failing.token().await, Err(ProviderError::InvalidRequest(m)) if m.contains("Token"))
        );
        let empty = TokenSource::command("true", vec![]);
        assert!(empty.token().await.is_err());
        assert!(
            TokenSource::command("gibt-es-nicht-4711", vec![])
                .token()
                .await
                .is_err()
        );
    }
}
