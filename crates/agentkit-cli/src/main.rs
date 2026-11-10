//! Binary zum Ausprobieren: `agentkit chat` streamt die Antwort ins Terminal.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use agentkit_cli::{CliError, Options, build_agent, chat_turn};
use agentkit_core::{Decision, FinishReason, SessionId, ToolCall};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agentkit",
    version,
    about = "AI-Agent-Framework: Kommandozeile zum Ausprobieren"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Gespräch mit dem Agenten; ohne PROMPT interaktiv, Zeile für Zeile.
    Chat {
        /// Einzelne Frage. Fehlt sie, liest das Programm Fragen von der Standardeingabe.
        prompt: Option<String>,
        /// Name des Modells.
        #[arg(long, env = "AGENTKIT_MODEL")]
        model: Option<String>,
        /// System-Prompt.
        #[arg(long)]
        system: Option<String>,
        /// Arbeitsverzeichnis, in dem die Tools Dateien lesen dürfen.
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Basis-URL der OpenAI-kompatiblen API.
        #[arg(long, env = "AGENTKIT_BASE_URL")]
        base_url: Option<String>,
    },
}

/// Fragt im Terminal nach, ob ein Tool-Aufruf ausgeführt werden darf.
fn ask_user(call: &ToolCall) -> Decision {
    eprint!("Tool {} mit {} ausführen? [j/N] ", call.name, call.input);
    let mut answer = String::new();
    match std::io::stdin().lock().read_line(&mut answer) {
        Ok(_)
            if matches!(
                answer.trim().to_lowercase().as_str(),
                "j" | "ja" | "y" | "yes"
            ) =>
        {
            Decision::Allow
        }
        _ => Decision::Deny,
    }
}

async fn chat(options: Options, prompt: Option<String>) -> Result<(), CliError> {
    let agent = build_agent(&options)?;
    let session = SessionId::new();
    let mut out = std::io::stdout();
    let mut approve = ask_user;
    let mut turn = async |prompt: &str| -> Result<(), CliError> {
        let outcome = chat_turn(&agent, session, prompt, &mut out, &mut approve).await?;
        if outcome.reason != FinishReason::Completed {
            eprintln!("Lauf beendet: {:?}", outcome.reason);
        }
        Ok(())
    };
    if let Some(prompt) = prompt {
        return turn(&prompt).await;
    }
    loop {
        eprint!("> ");
        std::io::stderr().flush()?;
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        if !line.trim().is_empty() {
            turn(line.trim()).await?;
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Chat {
            prompt,
            model,
            system,
            workdir,
            base_url,
        } => {
            chat(
                Options {
                    model,
                    system,
                    workdir,
                    base_url,
                },
                prompt,
            )
            .await
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Fehler: {error}");
            ExitCode::FAILURE
        }
    }
}
