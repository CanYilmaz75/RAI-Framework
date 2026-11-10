//! Tool `shell`: startet ein Programm direkt, ohne Shell dazwischen.

use std::process::Stdio;

use can_core::{ToolCtx, ToolError};
use can_macros::tool;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// So viele Bytes werden je Ausgabestrom höchstens behalten; der Rest wird verworfen.
const MAX_STREAM_BYTES: usize = 1024 * 1024;

/// Beendet beim Verwerfen die ganze Prozessgruppe, also auch Enkelprozesse.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.0.take() {
            // Negative Kennung: das Signal geht an alle Prozesse der Gruppe.
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{group}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Liest den Strom zu Ende, behält aber höchstens `MAX_STREAM_BYTES`.
async fn read_capped(reader: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let mut kept = Vec::new();
    let Some(mut reader) = reader else {
        return kept;
    };
    let mut buffer = [0u8; 8192];
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        let room = MAX_STREAM_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(room)]);
    }
    kept
}

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
    let mut command = Command::new(&program);
    command
        .args(args.unwrap_or_default())
        .current_dir(&cx.workdir)
        .env_clear()
        .env("PATH", search_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Eigene Prozessgruppe, damit ein Abbruch auch Kindeskinder trifft.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        ToolError::ForModel(format!("Programm nicht startbar: {program} ({error})"))
    })?;
    let mut guard = GroupGuard(child.id());
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let finished = async { tokio::join!(read_capped(stdout), read_capped(stderr), child.wait()) };
    let (stdout, stderr, status) = tokio::select! {
        finished = finished => finished,
        // Beim Verlassen beendet der Wächter die ganze Prozessgruppe.
        _ = cx.cancel.cancelled() => return Err(ToolError::ForModel("Abgebrochen.".into())),
    };
    // Regulär beendet: es gibt nichts mehr zu töten, und die Kennung könnte neu vergeben sein.
    guard.0 = None;
    let status =
        status.map_err(|error| ToolError::ForModel(format!("Programm fehlgeschlagen: {error}")))?;
    let code = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "Signal".to_owned());
    Ok(format!(
        "Exit-Code: {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&stdout).trim_end(),
        String::from_utf8_lossy(&stderr).trim_end()
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
    async fn huge_output_is_capped_but_the_program_still_finishes() {
        let dir = TempDir::new();
        let output = run(
            &dir,
            json!({"program": "head", "args": ["-c", "3000000", "/dev/zero"]}),
        )
        .await;
        assert!(output.starts_with("Exit-Code: 0"));
        assert!(output.len() < MAX_STREAM_BYTES + 200, "{}", output.len());
    }

    #[tokio::test]
    async fn unknown_program_is_an_error_for_the_model() {
        let dir = TempDir::new();
        let missing = shell
            .call(json!({"program": "gibt-es-nicht-4711"}), &dir.ctx())
            .await;
        assert!(matches!(missing, Err(ToolError::ForModel(m)) if m.contains("nicht startbar")));
    }

    #[tokio::test]
    async fn cancel_kills_the_whole_process_group() {
        let dir = TempDir::new();
        let cx = dir.ctx();
        let cancel = cx.cancel.clone();
        // Das Programm startet einen Enkelprozess und merkt sich dessen Kennung.
        let script = "sleep 300 & echo $! > enkel.pid; wait";
        let call = tokio::spawn(async move {
            shell
                .call(json!({"program": "sh", "args": ["-c", script]}), &cx)
                .await
        });
        let pid_file = dir.path().join("enkel.pid");
        while std::fs::read_to_string(&pid_file)
            .map(|text| text.trim().is_empty())
            .unwrap_or(true)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let grandchild = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        assert!(std::path::Path::new(&format!("/proc/{grandchild}")).exists());

        cancel.cancel();
        assert!(matches!(call.await.unwrap(), Err(ToolError::ForModel(m)) if m == "Abgebrochen."));
        for _ in 0..200 {
            let status =
                std::fs::read_to_string(format!("/proc/{grandchild}/stat")).unwrap_or_default();
            // Verschwunden oder Zombie (Zustand Z): der Prozess läuft nicht mehr.
            if status.is_empty() || status.contains(") Z ") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("der Enkelprozess {grandchild} lebt noch");
    }
}
