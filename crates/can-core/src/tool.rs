//! Tool-Trait: jedes Tool deklariert Wirkung, Zeitlimit und Ausgabegrenze.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::clock::Clock;
use crate::ids::{CallId, SessionId};
use crate::message::ToolOutput;
use crate::provider::{BoxError, Schema, ToolDecl};
use crate::secret::SecretsHandle;

/// Wirkungsklasse eines Tools; Grundlage für Policy und Parallelität.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    ReadOnly,
    Write,
    External,
}

/// Beschreibung eines Tools.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Schema,
    pub effect: Effect,
    pub idempotent: bool,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// Liefert das Tool vertrauliche Daten? Dann greift danach der Schutz vor Datenabfluss.
    #[serde(default)]
    pub confidential: bool,
}

impl ToolSpec {
    /// Spec mit sicheren Vorgaben: `Write`, nicht idempotent, 60 Sekunden, 64 KiB Ausgabe.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema: Schema(input_schema),
            effect: Effect::Write,
            idempotent: false,
            timeout: Duration::from_secs(60),
            max_output_bytes: 64 * 1024,
            confidential: false,
        }
    }

    /// Setzt die Wirkungsklasse.
    pub fn with_effect(mut self, effect: Effect) -> Self {
        self.effect = effect;
        self
    }

    /// Markiert das Tool als idempotent oder nicht.
    pub fn with_idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }

    /// Setzt das Zeitlimit pro Aufruf.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Markiert die Ausgaben des Tools als vertraulich.
    pub fn with_confidential(mut self, confidential: bool) -> Self {
        self.confidential = confidential;
        self
    }

    /// Der Teil der Spec, den das Modell sieht.
    pub fn decl(&self) -> ToolDecl {
        ToolDecl {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

/// Senke für Zwischenmeldungen eines Tools.
#[derive(Clone, Debug, Default)]
pub struct ProgressSink(Option<mpsc::UnboundedSender<String>>);

impl ProgressSink {
    /// Senke, die alle Meldungen verwirft.
    pub fn noop() -> Self {
        Self(None)
    }

    /// Senke samt Empfänger für die Meldungen.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(Some(tx)), rx)
    }

    /// Meldet einen Zwischenstand; ohne Empfänger passiert nichts.
    pub fn report(&self, message: impl Into<String>) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(message.into());
        }
    }
}

/// Kontext eines Tool-Aufrufs.
pub struct ToolCtx {
    pub session: SessionId,
    pub call_id: CallId,
    /// Bereits kanonisiert.
    pub workdir: PathBuf,
    pub cancel: CancellationToken,
    pub clock: Arc<dyn Clock>,
    pub progress: ProgressSink,
    pub secrets: SecretsHandle,
}

/// Fehler eines Tools.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Geht als Fehlerergebnis ans Modell.
    #[error("{0}")]
    ForModel(String),
    /// Beendet den Lauf.
    #[error("fataler Tool-Fehler: {0}")]
    Fatal(#[source] BoxError),
}

/// Ein Werkzeug, das das Modell aufrufen kann.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Beschreibung, Schema und Wirkung.
    fn spec(&self) -> &ToolSpec;

    /// Führt das Tool mit bereits geprüften Argumenten aus.
    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_spec_has_safe_defaults() {
        let spec = ToolSpec::new("t", "d", json!({"type": "object"}));
        assert_eq!(spec.effect, Effect::Write);
        assert!(!spec.idempotent);
        assert_eq!(spec.timeout, Duration::from_secs(60));
        assert_eq!(spec.max_output_bytes, 65_536);
    }

    #[test]
    fn builders_and_decl() {
        let spec = ToolSpec::new("t", "d", json!({"type": "object"}))
            .with_effect(Effect::ReadOnly)
            .with_idempotent(true)
            .with_timeout(Duration::from_secs(5));
        assert_eq!(
            (spec.effect, spec.idempotent, spec.timeout.as_secs()),
            (Effect::ReadOnly, true, 5)
        );
        let decl = spec.decl();
        assert_eq!((decl.name.as_str(), decl.description.as_str()), ("t", "d"));
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<ToolSpec>(&json).unwrap(), spec);
    }

    #[test]
    fn progress_sink_delivers_or_drops() {
        ProgressSink::noop().report("egal");
        let (sink, mut rx) = ProgressSink::channel();
        sink.report("halb fertig");
        assert_eq!(rx.try_recv().unwrap(), "halb fertig");
    }
}
