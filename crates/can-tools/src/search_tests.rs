use super::*;
use crate::testkit::TempDir;
use can_core::{Effect, Tool};
use serde_json::{Value, json};

fn project() -> TempDir {
    let dir = TempDir::new();
    dir.write("Cargo.toml", b"[package]\nname = \"demo\"\n");
    dir.write(
        "src/main.rs",
        b"fn main() {\n    println!(\"Hallo Welt\");\n}\n",
    );
    dir.write(
        "src/util/text.rs",
        b"pub fn hallo() -> &'static str {\n    \"hallo\"\n}\n",
    );
    dir.write("docs/notes.md", "Grüße: hallo zusammen\n".as_bytes());
    dir.write(".git/config", b"hallo aus git\n");
    dir.write("target/debug/out.rs", b"hallo aus target\n");
    dir.write("bild.bin", &[0xff, 0xfe, 0x00, 0x68]);
    dir
}

async fn text(tool: &dyn Tool, dir: &TempDir, args: Value) -> String {
    tool.call(args, &dir.ctx()).await.unwrap().as_text()
}

async fn error(tool: &dyn Tool, dir: &TempDir, args: Value) -> String {
    match tool.call(args, &dir.ctx()).await {
        Err(ToolError::ForModel(message)) => message,
        other => panic!("erwartet war ein Fehler fürs Modell: {other:?}"),
    }
}

#[test]
fn glob_patterns() {
    for (pattern, path, expected) in [
        ("*.rs", "main.rs", true),
        ("*.rs", "src/main.rs", false),
        ("src/*.rs", "src/main.rs", true),
        ("src/*.rs", "src/util/text.rs", false),
        ("**/*.rs", "src/util/text.rs", true),
        ("**/*.rs", "main.rs", true),
        ("src/**", "src/util/text.rs", true),
        ("src/**/text.?s", "src/util/text.rs", true),
        ("docs/*.md", "docs/notes.md", true),
        ("docs/*.md", "docs/notes.txt", false),
        ("*", "a/b", false),
        ("ma*n.rs", "main.rs", true),
    ] {
        assert_eq!(
            glob_matches(pattern, path),
            expected,
            "{pattern} gegen {path}"
        );
    }
}

#[tokio::test]
async fn ls_lists_entries_and_marks_folders() {
    let dir = project();
    assert_eq!(ls.spec().effect, Effect::ReadOnly);
    let root = text(&ls, &dir, json!({})).await;
    assert_eq!(root, ".git/\nCargo.toml\nbild.bin\ndocs/\nsrc/\ntarget/");
    assert_eq!(
        text(&ls, &dir, json!({"path": "src"})).await,
        "main.rs\nutil/"
    );
    assert!(
        error(&ls, &dir, json!({"path": "../"}))
            .await
            .contains("außerhalb")
    );
    assert!(
        error(&ls, &dir, json!({"path": "fehlt"}))
            .await
            .contains("nicht lesbar")
    );
    let empty = TempDir::new();
    assert_eq!(
        text(&ls, &empty, json!({})).await,
        "Keine Einträge gefunden."
    );
}

#[tokio::test]
async fn glob_finds_files_and_skips_hidden_and_build_folders() {
    let dir = project();
    assert_eq!(
        text(&glob, &dir, json!({"pattern": "**/*.rs"})).await,
        "src/main.rs\nsrc/util/text.rs"
    );
    assert_eq!(
        text(&glob, &dir, json!({"pattern": "*.toml"})).await,
        "Cargo.toml"
    );
    assert_eq!(
        text(&glob, &dir, json!({"pattern": "**/*.py"})).await,
        "Keine Dateien gefunden."
    );
    assert!(
        error(&glob, &dir, json!({"pattern": "../**"}))
            .await
            .contains("relativ")
    );
    assert!(
        error(&glob, &dir, json!({"pattern": "/etc/*"}))
            .await
            .contains("relativ")
    );
}

#[tokio::test]
async fn grep_reports_file_line_and_text() {
    let dir = project();
    let all = text(&grep, &dir, json!({"pattern": "hallo"})).await;
    assert_eq!(
        all,
        "docs/notes.md:1: Grüße: hallo zusammen\nsrc/util/text.rs:1: pub fn hallo() -> &'static str {\nsrc/util/text.rs:2:     \"hallo\""
    );
    let folded = text(
        &grep,
        &dir,
        json!({"pattern": "hallo", "ignore_case": true, "include": "**/*.rs"}),
    )
    .await;
    assert!(
        folded.starts_with("src/main.rs:2:") && folded.lines().count() == 3,
        "{folded}"
    );
    let scoped = text(&grep, &dir, json!({"pattern": "hallo", "path": "docs"})).await;
    assert_eq!(scoped.lines().count(), 1);
    let single = text(
        &grep,
        &dir,
        json!({"pattern": "name", "path": "Cargo.toml"}),
    )
    .await;
    assert_eq!(single, "Cargo.toml:2: name = \"demo\"");
    assert_eq!(
        text(&grep, &dir, json!({"pattern": "gibt es nicht"})).await,
        "Keine Treffer gefunden."
    );
    assert!(
        error(&grep, &dir, json!({"pattern": ""}))
            .await
            .contains("leer")
    );
    assert!(
        error(&grep, &dir, json!({"pattern": "x", "path": "/etc"}))
            .await
            .contains("außerhalb")
    );
}

#[tokio::test]
async fn results_are_capped() {
    let dir = TempDir::new();
    for n in 0..250 {
        dir.write(&format!("viele/datei{n:03}.txt"), b"treffer\n");
    }
    let found = text(&glob, &dir, json!({"pattern": "viele/*.txt"})).await;
    assert_eq!(found.lines().count(), 201);
    assert!(found.ends_with("… 50 weitere Dateien ausgelassen; das Muster enger fassen."));
    let hits = text(&grep, &dir, json!({"pattern": "treffer"})).await;
    assert!(hits.lines().count() == 201 && hits.contains("weitere Treffer ausgelassen"));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_never_followed() {
    let dir = project();
    let outside = TempDir::new();
    outside.write("geheim.rs", b"hallo von draussen\n");
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    assert!(
        !text(&glob, &dir, json!({"pattern": "**/*.rs"}))
            .await
            .contains("geheim")
    );
    assert!(
        !text(&grep, &dir, json!({"pattern": "draussen"}))
            .await
            .contains("geheim")
    );
}
