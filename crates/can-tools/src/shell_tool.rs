//! Tool `shell`: startet ein Programm direkt, ohne Shell dazwischen.

use std::process::Stdio;

use can_core::{ToolCtx, ToolError};
use can_macros::tool;
use tokio::process::Command;

/// Führt ein Programm im Arbeitsverzeichnis aus und liefert Exit-Code, Ausgabe und Fehlerausgabe.
/// Programm und Argumente werden getrennt übergeben; es gibt keine Shell-Syntax wie Pipes.
#[tool(effect = "external", timeout = 60)]
pub async fn shell(
    cx: &ToolCtx,
    program: String,
    args: Option<Vec<String>>,
) -> Result<String, ToolError> {
    // Bereinigte Umgebung: nur der Suchpfad bleibt, keine Schlüssel aus dem Elternprozess.
    let search_path =
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_owned());
    let child = Command::new(&program)
        .args(args.unwrap_or_default())
        .current_dir(&cx.workdir)
        .env_clear()
        .env("PATH", search_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            ToolError::ForModel(format!("Programm nicht startbar: {program} ({error})"))
        })?;
    let output = tokio::select! {
        output = child.wait_with_output() => output,
        // Beim Verlassen wird das Kind durch `kill_on_drop` beendet.
        _ = cx.cancel.cancelled() => return Err(ToolError::ForModel("Abgebrochen.".into())),
    };
    let output =
        output.map_err(|error| ToolError::ForModel(format!("Programm fehlgeschlagen: {error}")))?;
    let code = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "Signal".to_owned());
    Ok(format!(
        "Exit-Code: {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout).trim_end(),
        String::from_utf8_lossy(&output.stderr).trim_end()
    ))
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;
    use can_core::{Effect, Tool};
    use serde_json::json;
    use std::time::Duration;

    async fn run(dir: &TempDir, args: serde_json::Value) -> String {
        shell.call(args, &dir.ctx()).await.unwrap().as_text()
    }

    #[test]
    fn spec_is_external_with_timeout() {
        let spec = shell.spec();
        assert_eq!(
            (spec.effect, spec.idempotent, spec.timeout),
            (Effect::External, false, Duration::from_secs(60))
        );
        assert_eq!(spec.input_schema.0["required"], json!(["program"]));
    }

    #[tokio::test]
    async fn runs_in_the_workdir_and_reports_exit_code() {
        let dir = TempDir::new();
        dir.write("da.txt", b"x");
        let listing = run(&dir, json!({"program": "ls"})).await;
        assert!(
            listing.starts_with("Exit-Code: 0\n--- stdout ---\nda.txt"),
            "{listing}"
        );
        let failed = run(&dir, json!({"program": "ls", "args": ["fehlt"]})).await;
        assert!(
            !failed.starts_with("Exit-Code: 0") && failed.contains("fehlt"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn arguments_are_never_interpreted_by_a_shell() {
        let dir = TempDir::new();
        let echoed = run(
            &dir,
            json!({"program": "echo", "args": ["a; touch gehackt", "$HOME", "`id`"]}),
        )
        .await;
        assert!(echoed.contains("a; touch gehackt $HOME `id`"), "{echoed}");
        assert!(!dir.path().join("gehackt").exists());
    }

    #[tokio::test]
    async fn environment_is_cleaned() {
        let dir = TempDir::new();
        let environment = run(&dir, json!({"program": "env"})).await;
        let variables: Vec<&str> = environment
            .lines()
            .filter(|line| line.contains('=') && !line.starts_with("Exit-Code"))
            .map(|line| line.split('=').next().unwrap_or(""))
            .collect();
        assert_eq!(variables, ["PATH"], "{environment}");
    }

    #[tokio::test]
    async fn unknown_program_and_cancel_are_errors_for_the_model() {
        let dir = TempDir::new();
        let missing = shell
            .call(json!({"program": "gibt-es-nicht-4711"}), &dir.ctx())
            .await;
        assert!(matches!(missing, Err(ToolError::ForModel(m)) if m.contains("nicht startbar")));

        let cx = dir.ctx();
        cx.cancel.cancel();
        let cancelled = shell
            .call(json!({"program": "sleep", "args": ["30"]}), &cx)
            .await;
        assert!(matches!(cancelled, Err(ToolError::ForModel(m)) if m == "Abgebrochen."));
    }
}
