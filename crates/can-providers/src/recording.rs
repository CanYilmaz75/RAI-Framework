//! `Recording<P>`: schreibt die Antworten eines Providers mit, damit `FakeProvider` sie abspielt.
//!
//! Die Datei enthält pro Modellantwort eine Zeile JSON: die Liste ihrer Chunks.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use can_core::{Capabilities, Chunk, Provider, ProviderError, ProviderStream, Request};
use futures::StreamExt;

use crate::fake::FakeProvider;

/// Dekorator, der jede vollständige Antwort an eine Datei anhängt.
pub struct Recording<P> {
    inner: P,
    path: PathBuf,
    /// Verhindert, dass sich Zeilen gleichzeitiger Antworten vermischen.
    lock: Arc<Mutex<()>>,
}

impl<P: Provider> Recording<P> {
    /// Zeichnet die Antworten von `inner` in `path` auf; die Datei wird bei Bedarf angelegt.
    pub fn new(inner: P, path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }
}

fn write_line(path: &Path, lock: &Mutex<()>, chunks: &[Chunk]) -> Result<(), ProviderError> {
    let failed = |error: &dyn std::fmt::Display| {
        ProviderError::Protocol(format!("Aufzeichnung fehlgeschlagen: {error}"))
    };
    let line = serde_json::to_string(chunks).map_err(|error| failed(&error))?;
    let _guard = lock.lock().map_err(|error| failed(&error))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| failed(&error))?;
    writeln!(file, "{line}").map_err(|error| failed(&error))
}

#[async_trait]
impl<P: Provider> Provider for Recording<P> {
    fn capabilities(&self, model: &str) -> Capabilities {
        self.inner.capabilities(model)
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let inner = self.inner.stream(req).await?;
        let (path, lock) = (self.path.clone(), self.lock.clone());
        let mut seen = Vec::new();
        Ok(Box::pin(inner.map(move |item| {
            let chunk = item?;
            seen.push(chunk.clone());
            if matches!(chunk, Chunk::Done { .. }) {
                write_line(&path, &lock, &seen)?;
            }
            Ok(chunk)
        })))
    }
}

impl FakeProvider {
    /// Lädt eine mit [`Recording`] geschriebene Datei als Skript.
    pub fn from_recording(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut turns = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            turns.push(serde_json::from_str::<Vec<Chunk>>(line)?);
        }
        Ok(Self::from_turns(turns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::SessionId;
    use serde_json::json;

    async fn chunks(provider: &dyn Provider) -> Result<Vec<Chunk>, ProviderError> {
        let mut stream = provider.stream(Request::default()).await?;
        let mut all = Vec::new();
        while let Some(item) = stream.next().await {
            all.push(item?);
        }
        Ok(all)
    }

    #[tokio::test]
    async fn recorded_answers_replay_identically() {
        let path = std::env::temp_dir().join(format!("can-recording-{}.jsonl", SessionId::new()));
        let original = FakeProvider::new()
            .tool_call("read_file", json!({"path": "a"}))
            .text("fertig");
        let recording = Recording::new(original, &path);
        assert!(recording.capabilities("m").tools);
        let first = chunks(&recording).await.unwrap();
        let second = chunks(&recording).await.unwrap();
        assert!(
            matches!(chunks(&recording).await, Err(ProviderError::Protocol(_))),
            "Fehler werden nicht aufgezeichnet"
        );

        let replay = FakeProvider::from_recording(&path).unwrap();
        assert_eq!(chunks(&replay).await.unwrap(), first);
        assert_eq!(chunks(&replay).await.unwrap(), second);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn unwritable_file_surfaces_as_error_and_bad_files_do_not_load() {
        let recording = Recording::new(
            FakeProvider::new().text("x"),
            "/gibt/es/nicht/aufzeichnung.jsonl",
        );
        assert!(
            matches!(chunks(&recording).await, Err(ProviderError::Protocol(m)) if m.contains("Aufzeichnung"))
        );
        assert!(FakeProvider::from_recording("/gibt/es/nicht.jsonl").is_err());
        let path = std::env::temp_dir().join(format!("can-kaputt-{}.jsonl", SessionId::new()));
        std::fs::write(&path, "kein json\n").unwrap();
        assert!(FakeProvider::from_recording(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
