use super::*;
use can_core::{Decision, SessionId};

const FULL: &str = r#"
listen = "0.0.0.0:9000"
worker = "worker-7"

[store]
kind = "postgres"
url_env = "CAN_DATABASE_URL"

[[agents]]
name = "helfer"
model = "mein-modell"
system = "Du hilfst."
provider = "anthropic"
api_key_env = "MODELL_KEY"
tools = ["read_file", "http_fetch"]
http_allowed_hosts = ["example.com"]
max_steps = 20

[[agents.rules]]
tool = "http_fetch"
decision = "allow"

[[tenants]]
name = "acme"
max_concurrent_runs = 3
max_tokens = 500000

[[tenants.keys]]
key_env = "ACME_ADA_KEY"
user = "ada"
role = "admin"
"#;

fn env(name: &str) -> Option<String> {
    match name {
        "ACME_ADA_KEY" => Some("ein-langer-api-schluessel".to_owned()),
        "MODELL_KEY" => Some("modell-geheim".to_owned()),
        _ => None,
    }
}

#[test]
fn full_file_parses_and_builds_directory_and_agents() {
    let config = ServerConfig::parse(FULL).unwrap();
    assert_eq!(
        (config.listen.as_str(), config.worker.as_deref()),
        ("0.0.0.0:9000", Some("worker-7"))
    );
    assert!(
        matches!(&config.store, StoreConfig::Postgres { url_env } if url_env == "CAN_DATABASE_URL")
    );
    assert_eq!(
        config.agents[0].rules[0],
        Rule::tool("http_fetch").decide(Decision::Allow)
    );

    let directory = config.directory(env).unwrap();
    let ada = directory
        .authenticate(Some("Bearer ein-langer-api-schluessel"))
        .unwrap();
    assert_eq!((ada.tenant.as_str(), ada.role), ("acme", Role::Admin));
    assert_eq!(
        directory.quota("acme"),
        Quota {
            max_concurrent_runs: 3,
            max_tokens: 500_000
        }
    );

    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let agents = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(config.agents(&store, env))
        .unwrap();
    let tools: Vec<&str> = agents["helfer"]
        .config()
        .tools
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(tools, ["http_fetch", "read_file"]);
    assert_eq!(agents["helfer"].config().limits.max_steps, 20);
}

#[test]
fn defaults_and_rejections() {
    let config = ServerConfig::parse("").unwrap();
    assert_eq!(config.listen, "127.0.0.1:8080");
    assert!(matches!(config.store, StoreConfig::Memory));
    assert!(
        ServerConfig::parse("[[agents]]\nname = \"a\"\nmodel = \"m\"\ntools = [\"rm\"]").is_err()
    );
    assert!(
        ServerConfig::parse("[[agents]]\nname = \"a\"\nmodel = \"m\"\nprovider = \"x\"").is_err()
    );
    let twice = "[[agents]]\nname = \"a\"\nmodel = \"m\"\n[[agents]]\nname = \"a\"\nmodel = \"m\"";
    assert!(
        ServerConfig::parse(twice)
            .unwrap_err()
            .to_string()
            .contains("doppelt")
    );
    assert!(
        ServerConfig::parse("[[tenants]]\nname = \"a\"\n[[tenants.keys]]\nkey = \"geheim\"")
            .is_err()
    );
}

#[test]
fn missing_or_short_keys_are_refused() {
    let config = ServerConfig::parse(FULL).unwrap();
    assert!(
        config
            .directory(|_| None)
            .unwrap_err()
            .to_string()
            .contains("ACME_ADA_KEY")
    );
    assert!(config.directory(|_| Some("kurz".to_owned())).is_err());
}

#[tokio::test]
async fn stores_open_and_missing_database_url_is_reported() {
    let memory = ServerConfig::parse("").unwrap();
    let (store, sessions) = memory.open_store(env).await.unwrap();
    assert!(store.load(SessionId::new()).await.unwrap().is_empty());
    assert!(sessions.list("acme").await.unwrap().is_empty());
    let postgres = ServerConfig::parse(FULL).unwrap();
    assert!(
        postgres
            .open_store(env)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("CAN_DATABASE_URL")
    );
}

#[tokio::test]
async fn isolation_settings_reach_the_agent() {
    let text = r#"
encryption_key_env = "STORE_KEY"

[secrets]
kind = "files"
dir = "/run/secrets"

[[agents]]
name = "sicher"
model = "m"
tools = ["read_file", "shell", "http_fetch"]
confidential_tools = ["read_file"]
exfiltration_guard = "deny"
blocked_terms = ["Projekt Zeus"]

[agents.sandbox]
mode = "namespaces"
memory_mb = 256
"#;
    let config = ServerConfig::parse(text).unwrap();
    assert!(
        config.agents[0].isolate_workdir,
        "im Dienst ist das getrennte Arbeitsverzeichnis die Vorgabe"
    );
    assert!(config.secret_names().contains(&"STORE_KEY".to_owned()));
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let agents = config.agents(&store, env).await.unwrap();
    let agent = agents["sicher"].config();
    assert_eq!(agent.exfiltration_guard, Some(Decision::Deny));
    let confidential: Vec<&str> = agent
        .tools
        .iter()
        .filter(|spec| spec.confidential)
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(confidential, ["read_file"]);

    // Ohne den Schlüssel startet der Dienst nicht; mit einem falschen auch nicht.
    assert!(
        config
            .open_store(env)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("STORE_KEY")
    );
    let short = |name: &str| (name == "STORE_KEY").then(|| "abc".to_owned());
    assert!(config.open_store(short).await.is_err());
    let good = |name: &str| (name == "STORE_KEY").then(|| "00".repeat(32));
    assert!(config.open_store(good).await.is_ok());

    let both = "[[agents]]\nname = \"a\"\nmodel = \"m\"\n[agents.mcp.x]\ncommand = \"c\"\nurl = \"http://x\"";
    let error = ServerConfig::parse(both)
        .unwrap()
        .agents(&store, env)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("genau eines"));
}
