//! Helfer für die Tests der Tools: temporäres Arbeitsverzeichnis und Kontext.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentkit_core::{CallId, ManualClock, ProgressSink, SecretsHandle, SessionId, ToolCtx};
use tokio_util::sync::CancellationToken;

/// Temporäres Verzeichnis, das beim Verwerfen gelöscht wird.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agentkit-test-{}", SessionId::new()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn write(&self, name: &str, content: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    pub(crate) fn ctx(&self) -> ToolCtx {
        ToolCtx {
            session: SessionId::new(),
            call_id: CallId::new(),
            workdir: self.0.clone(),
            cancel: CancellationToken::new(),
            clock: Arc::new(ManualClock::default()),
            progress: ProgressSink::noop(),
            secrets: SecretsHandle::empty(),
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
