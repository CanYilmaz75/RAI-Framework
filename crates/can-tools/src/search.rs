//! Dateien finden und durchsuchen: `ls`, `glob` und `grep`, begrenzt auf das Arbeitsverzeichnis.
//!
//! Symlinks werden nie verfolgt, versteckte Ordner wie `.git` und `target` übersprungen.

use std::path::{Path, PathBuf};

use can_core::{ToolCtx, ToolError};
use can_macros::tool;

use crate::fs::resolve;

/// So viele Treffer liefert ein Aufruf höchstens.
const MAX_RESULTS: usize = 200;
/// Größere Dateien durchsucht `grep` nicht.
const MAX_GREP_BYTES: u64 = 2 * 1024 * 1024;

fn for_model(message: impl Into<String>) -> ToolError {
    ToolError::ForModel(message.into())
}

/// Passt `text` auf das Muster eines Pfadsegments? `*` steht für beliebig viele Zeichen,
/// `?` für genau eines.
fn segment_matches(pattern: &[char], text: &[char]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            segment_matches(&pattern[1..], text)
                || (!text.is_empty() && segment_matches(pattern, &text[1..]))
        }
        (Some('?'), Some(_)) => segment_matches(&pattern[1..], &text[1..]),
        (Some(expected), Some(actual)) if expected == actual => {
            segment_matches(&pattern[1..], &text[1..])
        }
        _ => false,
    }
}

/// Passt der Pfad (Segmente relativ zum Arbeitsverzeichnis) auf das Muster? `**` steht für
/// beliebig viele Ordner.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    fn walk(pattern: &[&str], path: &[&str]) -> bool {
        match (pattern.first(), path.first()) {
            (None, None) => true,
            (Some(&"**"), _) => {
                walk(&pattern[1..], path) || (!path.is_empty() && walk(pattern, &path[1..]))
            }
            (Some(segment), Some(name)) => {
                let (segment, name): (Vec<char>, Vec<char>) =
                    (segment.chars().collect(), name.chars().collect());
                segment_matches(&segment, &name) && walk(&pattern[1..], &path[1..])
            }
            _ => false,
        }
    }
    let pattern: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    walk(&pattern, &path)
}

/// Alle Dateien unter `root`, als Pfade relativ zu `base`, sortiert.
fn files_under(base: &Path, root: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let (path, name) = (
                entry.path(),
                entry.file_name().to_string_lossy().into_owned(),
            );
            // `file_type` folgt Symlinks nicht: ein Link zählt weder als Ordner noch als Datei.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !name.starts_with('.') && name != "target" && name != "node_modules" {
                    stack.push(path);
                }
            } else if kind.is_file()
                && let Ok(relative) = path.strip_prefix(base)
            {
                found.push((relative.to_string_lossy().replace('\\', "/"), path));
            }
        }
    }
    found.sort();
    found
}

fn limited(mut lines: Vec<String>, what: &str) -> String {
    if lines.is_empty() {
        return format!("Keine {what} gefunden.");
    }
    let total = lines.len();
    lines.truncate(MAX_RESULTS);
    let mut text = lines.join("\n");
    if total > MAX_RESULTS {
        text.push_str(&format!(
            "\n… {} weitere {what} ausgelassen; das Muster enger fassen.",
            total - MAX_RESULTS
        ));
    }
    text
}

/// Listet die Einträge eines Ordners im Arbeitsverzeichnis. Ordner enden mit `/`.
#[tool(effect = "read_only", idempotent)]
pub async fn ls(cx: &ToolCtx, path: Option<String>) -> Result<String, ToolError> {
    let shown = path.unwrap_or_else(|| ".".to_owned());
    let dir = resolve(&cx.workdir, &shown)?;
    let entries = std::fs::read_dir(&dir)
        .map_err(|error| for_model(format!("Ordner nicht lesbar: {shown} ({error})")))?;
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    names.sort();
    Ok(limited(names, "Einträge"))
}

/// Findet Dateien im Arbeitsverzeichnis nach Muster, etwa `src/**/*.rs`. `*` passt innerhalb
/// eines Namens, `**` über beliebig viele Ordner.
#[tool(effect = "read_only", idempotent)]
pub async fn glob(cx: &ToolCtx, pattern: String) -> Result<String, ToolError> {
    if pattern.starts_with('/') || pattern.split('/').any(|part| part == "..") {
        return Err(for_model(
            "Das Muster muss relativ zum Arbeitsverzeichnis sein und darf kein `..` enthalten.",
        ));
    }
    let base = cx.workdir.clone();
    let files = tokio::task::spawn_blocking(move || files_under(&base, &base))
        .await
        .map_err(|error| ToolError::Fatal(Box::new(error)))?;
    let matches = files
        .into_iter()
        .map(|(relative, _)| relative)
        .filter(|relative| glob_matches(&pattern, relative));
    Ok(limited(matches.collect(), "Dateien"))
}

/// Sucht Text in den Dateien des Arbeitsverzeichnisses und liefert `Datei:Zeile: Text`.
/// Gesucht wird der Text wörtlich, kein regulärer Ausdruck. `path` begrenzt die Suche auf einen
/// Ordner oder eine Datei, `include` auf Dateien, die auf ein Muster wie `**/*.rs` passen.
#[tool(effect = "read_only", idempotent)]
pub async fn grep(
    cx: &ToolCtx,
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    ignore_case: Option<bool>,
) -> Result<String, ToolError> {
    if pattern.is_empty() {
        return Err(for_model("Der Suchtext ist leer."));
    }
    let base = cx.workdir.clone();
    let root = resolve(&cx.workdir, path.as_deref().unwrap_or("."))?;
    let fold = ignore_case.unwrap_or(false);
    let search = move || {
        let needle = if fold {
            pattern.to_lowercase()
        } else {
            pattern
        };
        let files = if root.is_file() {
            let relative = root
                .strip_prefix(&base)
                .unwrap_or(&root)
                .to_string_lossy()
                .replace('\\', "/");
            vec![(relative, root)]
        } else {
            files_under(&base, &root)
        };
        let mut hits = Vec::new();
        for (relative, file) in files {
            if include
                .as_deref()
                .is_some_and(|include| !glob_matches(include, &relative))
            {
                continue;
            }
            if std::fs::metadata(&file)
                .map(|meta| meta.len() > MAX_GREP_BYTES)
                .unwrap_or(true)
            {
                continue;
            }
            // Was kein Text ist, wird übersprungen.
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for (number, line) in text.lines().enumerate() {
                let found = if fold {
                    line.to_lowercase().contains(&needle)
                } else {
                    line.contains(&needle)
                };
                if found {
                    hits.push(format!("{relative}:{}: {}", number + 1, line.trim_end()));
                    if hits.len() > MAX_RESULTS {
                        return hits;
                    }
                }
            }
        }
        hits
    };
    let hits = tokio::task::spawn_blocking(search)
        .await
        .map_err(|error| ToolError::Fatal(Box::new(error)))?;
    Ok(limited(hits, "Treffer"))
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
