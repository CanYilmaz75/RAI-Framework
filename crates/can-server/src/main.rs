//! `can-server`: startet den HTTP-Dienst aus einer Konfigurationsdatei.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use can_runtime::SystemClock;
use can_server::{App, RunManager, ServerConfig, router};

/// So oft sucht ein Worker nach Läufen, deren Besitzer ausgefallen ist.
const RECOVER_EVERY: Duration = Duration::from_secs(15);

async fn run(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig::parse(&std::fs::read_to_string(path)?)?;
    // Alle genannten Secrets einmal aus der Quelle holen; die Konfiguration nennt nur Namen.
    let secrets = config.secrets.resolve(&config.secret_names()).await?;
    let lookup = |name: &str| secrets.get(name).cloned();
    let directory = config.directory(lookup)?;
    let (store, sessions) = config.open_store(lookup).await?;
    let agents = config.agents(&store, lookup).await?;
    if agents.is_empty() {
        return Err("Die Konfiguration enthält keinen Agenten.".into());
    }
    let runs = RunManager::new(agents, store, sessions);
    let app = Arc::new(App {
        runs: runs.clone(),
        directory,
        clock: Arc::new(SystemClock),
    });

    // Unterbrochene Läufe übernehmen: gleich beim Start und danach regelmäßig.
    tokio::spawn(async move {
        loop {
            let taken = runs.recover().await;
            if taken > 0 {
                eprintln!("{taken} unterbrochene Läufe übernommen");
            }
            tokio::time::sleep(RECOVER_EVERY).await;
        }
    });

    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    eprintln!("can-server lauscht auf http://{}", listener.local_addr()?);
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("Signal empfangen: keine neuen Verbindungen mehr");
    };
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Fragt die Health-Adresse ab, ohne ein weiteres Programm im Image zu brauchen.
async fn health(url: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let Ok(mut socket) = tokio::net::TcpStream::connect(host).await else {
        return false;
    };
    let request = format!("GET /{path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n");
    let mut response = String::new();
    socket.write_all(request.as_bytes()).await.is_ok()
        && socket.read_to_string(&mut response).await.is_ok()
        && response.starts_with("HTTP/1.1 200")
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let first = arguments.next();
    // Für den Health-Check im Container: `can-server --health <url>`.
    if first.as_deref() == Some("--health") {
        return match health(&arguments.next().unwrap_or_default()).await {
            true => ExitCode::SUCCESS,
            false => ExitCode::FAILURE,
        };
    }
    let path = first.unwrap_or_else(|| "can-server.toml".to_owned());
    match run(&path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Fehler: {error}");
            ExitCode::FAILURE
        }
    }
}
