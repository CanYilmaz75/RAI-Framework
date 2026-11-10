//! MCP über stdio: der Server ist ein Kindprozess, eine JSON-Nachricht pro Zeile.

use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::{McpError, Transport};

struct Pipes {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    /// Hält den Prozess am Leben; `kill_on_drop` beendet ihn mit dem Transport.
    _child: Child,
}

/// Transport zu einem als Kindprozess gestarteten MCP-Server.
pub struct StdioTransport {
    pipes: Mutex<Pipes>,
}

fn io(error: impl std::fmt::Display) -> McpError {
    McpError::Io(error.to_string())
}

impl StdioTransport {
    /// Startet das Programm direkt, ohne Shell. Seine Fehlerausgabe geht an die eigene.
    pub fn spawn(program: &str, args: &[String]) -> Result<Self, McpError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| McpError::Io(format!("{program} nicht startbar: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io("keine Standardeingabe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io("keine Standardausgabe"))?;
        let lines = BufReader::new(stdout).lines();
        Ok(Self {
            pipes: Mutex::new(Pipes {
                stdin,
                lines,
                _child: child,
            }),
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&self, message: Value) -> Result<Option<Value>, McpError> {
        // Eine Anfrage nach der anderen: die Sperre hält, bis die Antwort da ist.
        let mut pipes = self.pipes.lock().await;
        pipes
            .stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .map_err(io)?;
        pipes.stdin.flush().await.map_err(io)?;
        let Some(id) = message.get("id") else {
            return Ok(None);
        };
        loop {
            let line = pipes.lines.next_line().await.map_err(io)?;
            let line = line.ok_or_else(|| io("der Server hat die Verbindung beendet"))?;
            if line.trim().is_empty() {
                continue;
            }
            let reply: Value = serde_json::from_str(&line)
                .map_err(|error| McpError::Protocol(format!("kein JSON vom Server: {error}")))?;
            // Benachrichtigungen und Anfragen des Servers werden übersprungen.
            if reply.get("id") == Some(id) && reply.get("method").is_none() {
                return Ok(Some(reply));
            }
        }
    }
}
