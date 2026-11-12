//! Binary zum Ausprobieren: `can chat` und `can resume`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use can_cli::config::ProviderKind;
use can_cli::eval::{Suite, run_suite};
use can_cli::inspect::inspect;
use can_cli::{
    Answer, App, CliError, Config, Options, Terminal, build_app, chat_turn, open_store,
    resume_turn, user_message,
};
use can_core::{FinishReason, SessionId, ToolCall};
use can_runtime::Outcome;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "can",
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
    #[arg(long, env = "CAN_MODEL")]
    model: Option<String>,
    /// System-Prompt.
    #[arg(long)]
    system: Option<String>,
    /// Arbeitsverzeichnis, in dem die Tools arbeiten dürfen.
    #[arg(long)]
    workdir: Option<PathBuf>,
    /// Basis-URL der OpenAI-kompatiblen API.
    #[arg(long, env = "CAN_BASE_URL")]
    base_url: Option<String>,
    /// Anbieter der API: openai (auch Ollama und andere kompatible) oder anthropic.
    #[arg(long, env = "CAN_PROVIDER")]
    provider: Option<ProviderKind>,
    /// Konfigurationsdatei; ohne Angabe gilt can.toml im aktuellen Verzeichnis, falls vorhanden.
    #[arg(long)]
    config: Option<PathBuf>,
    /// SQLite-Datei, in der die Sitzungen gespeichert werden (Vorgabe: can.db).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Modellantworten für ein späteres Replay in diese Datei schreiben.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Modellantworten aus dieser Aufzeichnung abspielen statt einen Anbieter zu fragen.
    #[arg(long)]
    replay: Option<PathBuf>,
}

impl Common {
    fn options(self) -> Result<Options, CliError> {
        let config = Config::load(self.config.as_deref())?;
        let db = self
            .db
            .or_else(|| config.db.clone())
            .unwrap_or_else(|| PathBuf::from("can.db"));
        Ok(Options {
            model: self.model,
            system: self.system,
            workdir: self.workdir,
            base_url: self.base_url,
            provider: self.provider,
            db: Some(db),
            record: self.record,
            replay: self.replay,
            config,
        })
    }

    async fn app(self) -> Result<App, CliError> {
        build_app(&self.options()?).await
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
        /// Bilddatei (png, jpg, gif, webp), die der ersten Frage beiliegt; mehrfach möglich.
        #[arg(long)]
        image: Vec<PathBuf>,
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
    /// Verzweigt eine Sitzung: eine neue übernimmt den Verlauf bis zu einer Position.
    Fork {
        session: SessionId,
        /// Letzte Position, die übernommen wird (siehe `can inspect`).
        #[arg(long)]
        at: u64,
        #[command(flatten)]
        common: Common,
    },
    /// Zeigt den Verlauf einer Sitzung als Zeitleiste aus dem Store.
    Inspect {
        session: SessionId,
        #[command(flatten)]
        common: Common,
    },
    /// Lässt eine Sammlung von Testfällen laufen; endet mit Fehler, wenn einer scheitert.
    Eval {
        /// TOML-Datei mit den Fällen.
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
}

/// Fragt im Terminal nach, ob ein Tool-Aufruf ausgeführt werden darf.
fn ask_user(call: &ToolCall) -> Answer {
    eprint!(
        "Tool {} mit {} ausführen? [j/N/e = Argumente ändern] ",
        call.name, call.input
    );
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return Answer::Deny;
    }
    match answer.trim().to_lowercase().as_str() {
        "j" | "ja" | "y" | "yes" => Answer::Allow,
        "e" => {
            eprint!("Neue Argumente als JSON: ");
            let mut line = String::new();
            let parsed = std::io::stdin()
                .lock()
                .read_line(&mut line)
                .ok()
                .and_then(|_| serde_json::from_str(line.trim()).ok());
            parsed.map(Answer::Edit).unwrap_or_else(|| {
                eprintln!("Kein gültiges JSON; Aufruf abgelehnt.");
                Answer::Deny
            })
        }
        _ => Answer::Deny,
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
    mut images: Vec<PathBuf>,
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
        report(&chat_turn(&app, session, user_message(&prompt, &images)?, terminal).await?);
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
            // Bilder liegen nur der ersten Frage bei.
            let input = user_message(line.trim(), &std::mem::take(&mut images))?;
            let terminal = Terminal {
                out: &mut out,
                approve: &mut approve,
            };
            report(&chat_turn(&app, session, input, terminal).await?);
        }
    }
}

async fn run(command: Command) -> Result<(), CliError> {
    match command {
        Command::Chat {
            prompt,
            session,
            image,
            common,
        } => chat(common.app().await?, session, prompt, image).await,
        Command::Fork {
            session,
            at,
            common,
        } => {
            let app = common.app().await?;
            let fork = app.agent.fork(session, at).await?;
            println!("{fork}");
            Ok(())
        }
        Command::Inspect { session, common } => {
            let store = open_store(&common.options()?)?;
            inspect(store.as_ref(), session, &mut std::io::stdout()).await
        }
        Command::Eval { file, common } => {
            let suite = Suite::parse(&std::fs::read_to_string(&file)?)?;
            let app = common.app().await?;
            match run_suite(&app, &suite, &mut std::io::stdout()).await? {
                (_, 0) => Ok(()),
                (_, failed) => Err(CliError::Config(format!("{failed} Fälle gescheitert"))),
            }
        }
        Command::Resume { session, common } => {
            let app = common.app().await?;
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
