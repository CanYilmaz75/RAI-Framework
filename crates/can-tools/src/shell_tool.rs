//! Tool `shell`: startet ein Programm direkt, ohne Shell dazwischen.

use std::process::Stdio;

use can_core::{Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use can_macros::tool;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::sandbox::Sandbox;

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

/// Führt das Programm aus, gegebenenfalls in der Sandbox, und liefert Exit-Code und Ausgaben.
async fn run(
    cx: &ToolCtx,
    program: String,
    args: Vec<String>,
    sandbox: &Sandbox,
) -> Result<String, ToolError> {
    // Bereinigte Umgebung: nur der Suchpfad bleibt, keine Schlüssel aus dem Elternprozess.
    let search_path =
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_owned());
    let (launcher, arguments) = sandbox.command(&program, &args, &cx.workdir);
    let mut command = Command::new(&launcher);
    command
        .args(arguments)
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

/// Führt ein Programm im Arbeitsverzeichnis aus und liefert Exit-Code, Ausgabe und Fehlerausgabe.
/// Programm und Argumente werden getrennt übergeben; es gibt keine Shell-Syntax wie Pipes.
#[tool(effect = "external", timeout = 60)]
pub async fn shell(
    cx: &ToolCtx,
    program: String,
    args: Option<Vec<String>>,
) -> Result<String, ToolError> {
    run(cx, program, args.unwrap_or_default(), &Sandbox::default()).await
}

/// Wie [`shell`], aber jedes Programm läuft in der angegebenen Sandbox.
pub struct SandboxedShell {
    sandbox: Sandbox,
}

impl SandboxedShell {
    /// Tool `shell` mit Sandbox.
    pub fn new(sandbox: Sandbox) -> Self {
        Self { sandbox }
    }
}

#[async_trait::async_trait]
impl Tool for SandboxedShell {
    fn spec(&self) -> &ToolSpec {
        shell.spec()
    }

    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let invalid =
            |error: serde_json::Error| ToolError::ForModel(format!("Ungültige Argumente: {error}"));
        let program: String = serde_json::from_value(args["program"].clone()).map_err(invalid)?;
        let arguments: Option<Vec<String>> =
            serde_json::from_value(args["args"].clone()).map_err(invalid)?;
        run(cx, program, arguments.unwrap_or_default(), &self.sandbox)
            .await
            .map(ToolOutput::text)
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;
    use can_core::Effect;
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

    /// Ob `unshare` hier Namespaces anlegen darf; sonst werden die Sandbox-Tests übersprungen.
    fn namespaces_available() -> bool {
        let probe = std::process::Command::new("unshare")
            .args(["--net", "--pid", "--fork", "--mount-proc", "true"])
            .output();
        let ok = probe.map(|output| output.status.success()).unwrap_or(false);
        if !ok {
            eprintln!("unshare darf hier keine Namespaces anlegen: Sandbox-Test übersprungen");
        }
        ok
    }

    #[tokio::test]
    async fn sandbox_has_no_network_and_its_own_process_table() {
        if !namespaces_available() {
            return;
        }
        let dir = TempDir::new();
        let tool = SandboxedShell::new(Sandbox::namespaces());
        assert_eq!(tool.spec().name, "shell");
        // Im neuen Netzwerk-Namespace gibt es nur die (abgeschaltete) Loopback-Schnittstelle.
        let script = "grep -c : /proc/net/dev; ls /proc | grep -c '^[0-9]'; echo $$";
        let output = tool
            .call(json!({"program": "sh", "args": ["-c", script]}), &dir.ctx())
            .await
            .unwrap()
            .as_text();
        let lines: Vec<&str> = output.lines().skip(2).take(3).collect();
        assert_eq!(lines[0], "1", "nur lo: {output}");
        assert!(
            lines[1].parse::<u32>().unwrap() <= 4,
            "eigene Prozesstabelle: {output}"
        );
        let outside = run(
            &dir,
            json!({"program": "sh", "args": ["-c", "grep -c : /proc/net/dev"]}),
        )
        .await;
        assert_ne!(
            outside.lines().nth(2),
            Some("1"),
            "ohne Sandbox ist das Netzwerk sichtbar"
        );
    }

    #[tokio::test]
    async fn sandbox_enforces_the_memory_limit_and_validates_arguments() {
        let dir = TempDir::new();
        let bad = SandboxedShell::new(Sandbox::default())
            .call(json!({"program": 7}), &dir.ctx())
            .await;
        assert!(matches!(bad, Err(ToolError::ForModel(m)) if m.contains("Ungültige Argumente")));
        if !namespaces_available() {
            return;
        }
        let tool = SandboxedShell::new(Sandbox {
            memory_mb: Some(64),
            ..Sandbox::namespaces()
        });
        // Die Shell hält 100 MiB in einer Variablen: mit 64 MiB Grenze scheitert das.
        let script = "a=$(head -c 100000000 /dev/zero | tr '\\0' x); echo ${#a}";
        let hog = json!({"program": "sh", "args": ["-c", script]});
        let limited = tool.call(hog.clone(), &dir.ctx()).await.unwrap().as_text();
        let free = run(&dir, hog).await;
        assert!(free.contains("--- stdout ---\n100000000"), "{free}");
        assert!(!limited.contains("100000000"), "{limited}");
    }
}
