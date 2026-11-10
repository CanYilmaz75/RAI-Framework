//! Binary zum Ausprobieren: `agentkit chat` und `agentkit resume`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use agentkit_cli::{App, CliError, Options, Terminal, build_app, chat_turn, resume_turn};
use agentkit_core::{Decision, FinishReason, SessionId, ToolCall};
use agentkit_runtime::Outcome;
use clap::{Args, Parser, Subcommand};

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

#[derive(Args)]
struct Common {
    /// Name des Modells.
    #[arg(long, env = "AGENTKIT_MODEL")]
    model: Option<String>,
    /// System-Prompt.
    #[arg(long)]
    system: Option<String>,
    /// Arbeitsverzeichnis, in dem die Tools arbeiten dürfen.
    #[arg(long)]
    workdir: Option<PathBuf>,
    /// Basis-URL der OpenAI-kompatiblen API.
    #[arg(long, env = "AGENTKIT_BASE_URL")]
    base_url: Option<String>,
    /// SQLite-Datei, in der die Sitzungen gespeichert werden.
    #[arg(long, default_value = "agentkit.db")]
    db: PathBuf,
    /// Modellantworten für ein späteres Replay in diese Datei schreiben.
    #[arg(long)]
    record: Option<PathBuf>,
}

impl Common {
    fn app(self) -> Result<App, CliError> {
        build_app(&Options {
            model: self.model,
            system: self.system,
            workdir: self.workdir,
            base_url: self.base_url,
            db: Some(self.db),
            record: self.record,
        })
    }
}

#[derive(Subcommand)]
enum Command {
    /// Gespräch mit dem Agenten; ohne PROMPT interaktiv, Zeile für Zeile.
    Chat {
        /// Einzelne Frage. Fehlt sie, liest das Programm Fragen von der Standardeingabe.
        prompt: Option<String>,
        /// Vorhandene Sitzung weiterführen statt eine neue zu beginnen.
        #[arg(long)]
        session: Option<SessionId>,
        #[command(flatten)]
        common: Common,
    },
    /// Setzt einen unterbrochenen Lauf am letzten gespeicherten Zustand fort.
    Resume {
        /// Kennung der Sitzung, wie `chat` sie beim Start ausgibt.
        session: SessionId,
        #[command(flatten)]
        common: Common,
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

fn report(outcome: &Outcome) {
    if outcome.reason != FinishReason::Completed {
        eprintln!("Lauf beendet: {:?}", outcome.reason);
    }
}

async fn chat(
    app: App,
    session: Option<SessionId>,
    prompt: Option<String>,
) -> Result<(), CliError> {
    let session = session.unwrap_or_default();
    eprintln!("Sitzung: {session}");
    let mut out = std::io::stdout();
    let mut approve = ask_user;
    if let Some(prompt) = prompt {
        let terminal = Terminal {
            out: &mut out,
            approve: &mut approve,
        };
        report(&chat_turn(&app, session, &prompt, terminal).await?);
        return Ok(());
    }
    loop {
        eprint!("> ");
        std::io::stderr().flush()?;
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        if !line.trim().is_empty() {
            let terminal = Terminal {
                out: &mut out,
                approve: &mut approve,
            };
            report(&chat_turn(&app, session, line.trim(), terminal).await?);
        }
    }
}

async fn run(command: Command) -> Result<(), CliError> {
    match command {
        Command::Chat {
            prompt,
            session,
            common,
        } => chat(common.app()?, session, prompt).await,
        Command::Resume { session, common } => {
            let app = common.app()?;
            let (mut out, mut approve) = (std::io::stdout(), ask_user);
            let terminal = Terminal {
                out: &mut out,
                approve: &mut approve,
            };
            report(&resume_turn(&app, session, terminal).await?);
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse().command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Fehler: {error}");
            ExitCode::FAILURE
        }
    }
}
