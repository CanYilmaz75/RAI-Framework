//! Pfadprüfung und Dateizugriff für die Datei-Tools.

use std::path::{Component, Path, PathBuf};

use agentkit_core::{ToolCtx, ToolError};

/// Größte Datei, die `read_text` liest.
pub const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

fn for_model(message: impl Into<String>) -> ToolError {
    ToolError::ForModel(message.into())
}

/// Löst `path` relativ zum Arbeitsverzeichnis auf und stellt sicher, dass das Ziel darin liegt.
///
/// Symlinks werden aufgelöst, bevor geprüft wird. Der Pfad darf auf etwas zeigen, das noch
/// nicht existiert; dann wird der längste vorhandene Vorfahr geprüft.
pub fn resolve(workdir: &Path, path: &str) -> Result<PathBuf, ToolError> {
    if path.trim().is_empty() {
        return Err(for_model("Der Pfad ist leer."));
    }
    let joined = workdir.join(path);
    let mut existing = joined.as_path();
    let mut missing = Vec::new();
    // `symlink_metadata` statt `exists`: ein Symlink ins Leere zählt als vorhanden und
    // scheitert dann beim Auflösen, statt später stillschweigend verfolgt zu werden.
    while existing.symlink_metadata().is_err() {
        match (existing.components().next_back(), existing.parent()) {
            (Some(Component::Normal(name)), Some(parent)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return Err(for_model(format!("Ungültiger Pfad: {path}"))),
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|error| for_model(format!("Pfad nicht auflösbar: {path} ({error})")))?;
    resolved.extend(missing.into_iter().rev());
    if !resolved.starts_with(workdir) {
        return Err(for_model(format!(
            "Der Pfad liegt außerhalb des Arbeitsverzeichnisses: {path}"
        )));
    }
    Ok(resolved)
}

/// Liest eine Textdatei im Arbeitsverzeichnis. Ungültiges UTF-8 wird ersetzt.
pub async fn read_text(cx: &ToolCtx, path: &str) -> Result<String, ToolError> {
    let resolved = resolve(&cx.workdir, path)?;
    let metadata = tokio::fs::metadata(&resolved)
        .await
        .map_err(|error| for_model(format!("Datei nicht lesbar: {path} ({error})")))?;
    if !metadata.is_file() {
        return Err(for_model(format!("Keine Datei: {path}")));
    }
    if metadata.len() > MAX_READ_BYTES {
        return Err(for_model(format!(
            "Datei zu groß: {path} hat {} Bytes, erlaubt sind {MAX_READ_BYTES}.",
            metadata.len()
        )));
    }
    let bytes = tokio::fs::read(&resolved)
        .await
        .map_err(|error| for_model(format!("Datei nicht lesbar: {path} ({error})")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Schreibt eine Textdatei im Arbeitsverzeichnis und legt fehlende Ordner an.
pub async fn write_text(cx: &ToolCtx, path: &str, content: &str) -> Result<String, ToolError> {
    let resolved = resolve(&cx.workdir, path)?;
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| for_model(format!("Ordner nicht anlegbar: {path} ({error})")))?;
    }
    tokio::fs::write(&resolved, content)
        .await
        .map_err(|error| for_model(format!("Datei nicht schreibbar: {path} ({error})")))?;
    Ok(format!("{} Bytes nach {path} geschrieben.", content.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;

    fn message(result: Result<PathBuf, ToolError>) -> String {
        match result {
            Err(ToolError::ForModel(message)) => message,
            other => panic!("erwartet war ein Fehler fürs Modell: {other:?}"),
        }
    }

    #[test]
    fn resolves_existing_and_new_paths_inside_workdir() {
        let dir = TempDir::new();
        dir.write("sub/a.txt", b"x");
        assert_eq!(
            resolve(dir.path(), "sub/a.txt").unwrap(),
            dir.path().join("sub/a.txt")
        );
        assert_eq!(
            resolve(dir.path(), "sub/../sub/a.txt").unwrap(),
            dir.path().join("sub/a.txt")
        );
        assert_eq!(
            resolve(dir.path(), "neu/tief/b.txt").unwrap(),
            dir.path().join("neu/tief/b.txt")
        );
        let absolute = dir.path().join("sub/a.txt");
        assert_eq!(
            resolve(dir.path(), absolute.to_str().unwrap()).unwrap(),
            absolute
        );
    }

    #[test]
    fn rejects_escapes() {
        let dir = TempDir::new();
        dir.write("sub/a.txt", b"x");
        assert!(message(resolve(dir.path(), "../a.txt")).contains("außerhalb"));
        assert!(message(resolve(dir.path(), "/etc/passwd")).contains("außerhalb"));
        assert!(message(resolve(dir.path(), "sub/../../x")).contains("außerhalb"));
        assert!(message(resolve(dir.path(), "neu/../../x")).contains("Ungültiger Pfad"));
        assert!(message(resolve(dir.path(), "  ")).contains("leer"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_leave_the_workdir() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        outside.write("geheim.txt", b"geheim");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("fehlt"), dir.path().join("leer")).unwrap();
        assert!(message(resolve(dir.path(), "link/geheim.txt")).contains("außerhalb"));
        assert!(message(resolve(dir.path(), "link/neu.txt")).contains("außerhalb"));
        assert!(message(resolve(dir.path(), "leer")).contains("nicht auflösbar"));
    }

    #[tokio::test]
    async fn write_text_creates_folders_and_stays_inside() {
        let dir = TempDir::new();
        let cx = dir.ctx();
        assert_eq!(
            write_text(&cx, "neu/a.txt", "hallo").await.unwrap(),
            "5 Bytes nach neu/a.txt geschrieben."
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("neu/a.txt")).unwrap(),
            "hallo"
        );
        assert!(
            matches!(write_text(&cx, "../raus.txt", "x").await, Err(ToolError::ForModel(m)) if m.contains("außerhalb"))
        );
        assert!(
            matches!(write_text(&cx, "neu", "x").await, Err(ToolError::ForModel(m)) if m.contains("nicht schreibbar"))
        );
    }

    #[tokio::test]
    async fn read_text_reads_and_reports_problems() {
        let dir = TempDir::new();
        dir.write("a.txt", "grüße".as_bytes());
        dir.write("bin.dat", &[b'a', 0xff, b'b']);
        let cx = dir.ctx();
        assert_eq!(read_text(&cx, "a.txt").await.unwrap(), "grüße");
        assert_eq!(read_text(&cx, "bin.dat").await.unwrap(), "a\u{fffd}b");
        assert!(
            matches!(read_text(&cx, "fehlt.txt").await, Err(ToolError::ForModel(m)) if m.contains("nicht lesbar"))
        );
        assert!(
            matches!(read_text(&cx, ".").await, Err(ToolError::ForModel(m)) if m.contains("Keine Datei"))
        );
        assert!(matches!(
            read_text(&cx, "../x").await,
            Err(ToolError::ForModel(_))
        ));
    }
}
