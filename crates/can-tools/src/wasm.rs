//! Tools als WebAssembly-Module (WASI Preview 1) in einer Sandbox mit Freigaben pro Tool.
//!
//! Ein Modul bekommt die Argumente als JSON auf der Standardeingabe und schreibt sein Ergebnis
//! auf die Standardausgabe. Es sieht nur, was freigegeben ist:
//!
//! - **Dateien:** keine, das Arbeitsverzeichnis lesend oder lesend und schreibend (als `/work`).
//! - **Netzwerk:** WASI Preview 1 kennt keine Verbindungen. Abrufe gehen nur über die
//!   Host-Funktion `can.http_get`, und die prüft die Freigabeliste des Tools, die Sperre privater
//!   Adressen und jede Weiterleitung.
//! - **Secrets:** nur ausdrücklich genannte Schlüssel, als Umgebungsvariablen.
//! - **Rechenzeit und Speicher:** begrenzt über Treibstoff (Fuel) und eine Speichergrenze.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use can_core::{Effect, Schema, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::Value;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{FsPerms, I32Exit, WasiCtxBuilder};

use crate::HttpFetch;

/// So viele Bytes Ausgabe behält die Sandbox höchstens.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Zugriff auf das Arbeitsverzeichnis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccess {
    #[default]
    None,
    Read,
    ReadWrite,
}

/// Beschreibung und Freigaben eines WASM-Tools.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WasmToolConfig {
    pub name: String,
    pub description: String,
    /// Pfad zur `.wasm`-Datei (WASI Preview 1).
    pub module: PathBuf,
    /// JSON-Schema der Argumente.
    #[serde(default = "object_schema")]
    pub input_schema: Value,
    /// Wirkungsklasse für die Policy; ohne Angabe `write`, also mit Rückfrage.
    #[serde(default = "default_effect")]
    pub effect: Effect,
    #[serde(default)]
    pub files: FileAccess,
    /// Hosts, die das Modul über `can.http_get` abrufen darf (`*` als Platzhalter).
    #[serde(default)]
    pub network_hosts: Vec<String>,
    /// Namen der Secrets, die das Modul als Umgebungsvariablen sieht.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Speichergrenze in MiB.
    #[serde(default = "default_memory")]
    pub memory_mb: u64,
    /// Rechenbudget in Millionen Fuel-Einheiten (grob Millionen Wasm-Befehle).
    #[serde(default = "default_fuel")]
    pub fuel_millions: u64,
}

fn object_schema() -> Value {
    serde_json::json!({"type": "object"})
}

fn default_effect() -> Effect {
    Effect::Write
}

fn default_memory() -> u64 {
    64
}

fn default_fuel() -> u64 {
    1_000
}

/// Zustand einer Ausführung.
struct Sandbox {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
    fetcher: Option<Arc<HttpFetch>>,
}

/// Ein WebAssembly-Modul als Tool.
pub struct WasmTool {
    spec: ToolSpec,
    config: WasmToolConfig,
    engine: Engine,
    module: Module,
    fetcher: Option<Arc<HttpFetch>>,
}

fn fatal(message: impl std::fmt::Display) -> ToolError {
    ToolError::Fatal(message.to_string().into())
}

impl WasmTool {
    /// Lädt und übersetzt das Modul. Fehler hier sind Konfigurationsfehler.
    pub fn load(config: WasmToolConfig) -> Result<Self, ToolError> {
        let mut engine_config = Config::new();
        engine_config.consume_fuel(true);
        let engine = Engine::new(&engine_config).map_err(fatal)?;
        let module = Module::from_file(&engine, &config.module).map_err(|error| {
            fatal(format!(
                "WASM-Modul {} nicht ladbar: {error}",
                config.module.display()
            ))
        })?;
        let spec = ToolSpec::new(
            &config.name,
            &config.description,
            config.input_schema.clone(),
        )
        .with_effect(config.effect);
        let spec = ToolSpec {
            input_schema: Schema(config.input_schema.clone()),
            ..spec
        };
        let fetcher = (!config.network_hosts.is_empty())
            .then(|| Arc::new(HttpFetch::new(config.network_hosts.clone())));
        Ok(Self {
            spec,
            config,
            engine,
            module,
            fetcher,
        })
    }

    /// Erlaubt Abrufe auch an private Adressen; nur für Tests und bewusst interne Dienste.
    pub fn allow_private_networks(mut self) -> Self {
        self.fetcher = self.fetcher.map(|_| {
            Arc::new(HttpFetch::new(self.config.network_hosts.clone()).allow_private_networks(true))
        });
        self
    }
}

/// Alles, was eine Ausführung braucht, ohne Verweise auf den Aufrufer.
struct Job {
    engine: Engine,
    module: Module,
    config: WasmToolConfig,
    fetcher: Option<Arc<HttpFetch>>,
    input: Vec<u8>,
    workdir: PathBuf,
    secrets: Vec<(String, String)>,
    handle: tokio::runtime::Handle,
}

impl Job {
    fn run(self) -> Result<ToolOutput, ToolError> {
        let Job {
            engine,
            module,
            config,
            fetcher,
            input,
            workdir,
            secrets,
            handle,
        } = self;
        let stdout = MemoryOutputPipe::new(MAX_OUTPUT_BYTES);
        let stderr = MemoryOutputPipe::new(64 * 1024);
        let mut builder = WasiCtxBuilder::new();
        builder
            .stdin(MemoryInputPipe::new(input))
            .stdout(stdout.clone())
            .stderr(stderr.clone());
        for (name, value) in &secrets {
            builder.env(name, value);
        }
        let permissions = match config.files {
            FileAccess::None => None,
            FileAccess::Read => Some(FsPerms::ReadOnly),
            FileAccess::ReadWrite => Some(FsPerms::ReadWrite),
        };
        if let Some(perms) = permissions {
            builder
                .preopened_dir(&workdir, "/work", perms)
                .map_err(fatal)?;
        }
        let limits = StoreLimitsBuilder::new()
            .memory_size(
                usize::try_from(config.memory_mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX),
            )
            .build();
        let sandbox = Sandbox {
            wasi: builder.build_p1(),
            limits,
            fetcher,
        };
        let mut store = Store::new(&engine, sandbox);
        store.limiter(|sandbox| &mut sandbox.limits);
        store
            .set_fuel(config.fuel_millions.saturating_mul(1_000_000))
            .map_err(fatal)?;

        let mut linker: Linker<Sandbox> = Linker::new(&engine);
        p1::add_to_linker_sync(&mut linker, |sandbox| &mut sandbox.wasi).map_err(fatal)?;
        let fetch = move |mut caller: Caller<'_, Sandbox>,
                          url_ptr: i32,
                          url_len: i32,
                          out_ptr: i32,
                          out_cap: i32|
              -> i64 {
            http_get(&mut caller, &handle, [url_ptr, url_len, out_ptr, out_cap])
        };
        linker.func_wrap("can", "http_get", fetch).map_err(fatal)?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|error| fatal(format!("WASM-Modul nicht startbar: {error}")))?;
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(fatal)?;
        let result = start.call(&mut store, ());
        let stderr_text = String::from_utf8_lossy(&stderr.contents())
            .trim()
            .to_owned();
        let exit = match result {
            Ok(()) => 0,
            Err(error) => match error.downcast_ref::<I32Exit>() {
                Some(exit) => exit.0,
                None if store.get_fuel().is_ok_and(|fuel| fuel == 0) => {
                    return Err(ToolError::ForModel(
                        "Das Rechenbudget des WASM-Tools ist aufgebraucht.".into(),
                    ));
                }
                None => {
                    return Err(ToolError::ForModel(format!(
                        "Das WASM-Tool ist abgestürzt: {error}"
                    )));
                }
            },
        };
        let text = String::from_utf8_lossy(&stdout.contents()).into_owned();
        if exit != 0 {
            let detail = if stderr_text.is_empty() {
                text.trim().to_owned()
            } else {
                stderr_text
            };
            return Err(ToolError::ForModel(format!(
                "Das WASM-Tool endete mit Code {exit}: {detail}"
            )));
        }
        Ok(ToolOutput::text(text))
    }
}

/// Host-Funktion `can.http_get(url_ptr, url_len, out_ptr, out_cap) -> i64`. Sie schreibt die
/// Antwort (oder die Fehlermeldung) in den Speicher des Moduls und liefert
/// `(status << 32) | geschriebene_bytes`: Status 0 heißt erfolgreich, 1 verweigert oder
/// fehlgeschlagen, 2 erfolgreich, aber auf den Puffer gekürzt.
fn http_get(
    caller: &mut Caller<'_, Sandbox>,
    handle: &tokio::runtime::Handle,
    args: [i32; 4],
) -> i64 {
    const FAILED: i64 = 1 << 32;
    let [url_ptr, url_len, out_ptr, out_cap] =
        args.map(|value| usize::try_from(value).unwrap_or(0));
    let Some(memory) = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
    else {
        return FAILED;
    };
    let mut url = vec![0u8; url_len.min(8192)];
    if memory.read(&mut *caller, url_ptr, &mut url).is_err() {
        return FAILED;
    }
    let url = String::from_utf8_lossy(&url).into_owned();
    let (status, body) = match caller.data().fetcher.clone() {
        None => (
            1,
            "Kein Netzwerk: für dieses Tool sind keine Hosts freigegeben.".to_owned(),
        ),
        Some(fetcher) => match handle.block_on(fetcher.fetch(&url)) {
            Ok(text) => (0, text),
            Err(error) => (1, error.to_string()),
        },
    };
    let bytes = body.into_bytes();
    let written = bytes.len().min(out_cap);
    if memory
        .write(&mut *caller, out_ptr, &bytes[..written])
        .is_err()
    {
        return FAILED;
    }
    let status = if status == 0 && bytes.len() > out_cap {
        2
    } else {
        status
    };
    (status << 32) | i64::try_from(written).unwrap_or(0)
}

#[async_trait]
impl Tool for WasmTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let secrets = self
            .config
            .secrets
            .iter()
            .filter_map(|name| {
                cx.secrets
                    .get(name)
                    .map(|value| (name.clone(), value.to_owned()))
            })
            .collect();
        let job = Job {
            engine: self.engine.clone(),
            module: self.module.clone(),
            config: self.config.clone(),
            fetcher: self.fetcher.clone(),
            input: serde_json::to_vec(&args).map_err(fatal)?,
            workdir: cx.workdir.clone(),
            secrets,
            handle: tokio::runtime::Handle::current(),
        };
        // Wasmtime läuft synchron auf einem Blocking-Thread. Ein Abbruch beendet den Aufruf für
        // den Lauf sofort; das Modul selbst endet spätestens, wenn sein Rechenbudget verbraucht ist.
        let task = tokio::task::spawn_blocking(move || job.run());
        tokio::select! {
            result = task => result.map_err(fatal)?,
            _ = cx.cancel.cancelled() => Err(ToolError::ForModel("Abgebrochen.".into())),
        }
    }
}

#[cfg(test)]
#[path = "wasm_tests.rs"]
mod tests;
