use super::*;
use crate::testkit::TempDir;
use can_core::{Secret, SecretsHandle};
use serde_json::json;
use std::path::Path;
use std::sync::OnceLock;

fn config(module: &Path) -> WasmToolConfig {
    WasmToolConfig {
        name: "demo".into(),
        description: "Beispiel".into(),
        module: module.to_path_buf(),
        input_schema: json!({"type": "object"}),
        effect: Effect::ReadOnly,
        files: FileAccess::None,
        network_hosts: Vec::new(),
        secrets: Vec::new(),
        memory_mb: 64,
        fuel_millions: 200,
    }
}

/// Schreibt ein Modul im Textformat in eine Datei; Wasmtime übersetzt es beim Laden.
fn wat(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    dir.write(name, body.as_bytes())
}

/// Modul, das einen festen Text auf die Standardausgabe schreibt.
const HELLO: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "hallo aus wasm")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 16))
    (i32.store (i32.const 4) (i32.const 14))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#;

/// Modul, das endlos rechnet.
const SPIN: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "_start") (loop $l (br $l))))"#;

/// Modul, das `can.http_get` aufruft und die Antwort samt Status ausgibt.
const FETCH: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "can" "http_get" (func $get (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 2)
  (data (i32.const 100) "http://localhost/")
  (func (export "_start") (local $r i64)
    (local.set $r (call $get (i32.const 100) (i32.const 17) (i32.const 1024) (i32.const 60000)))
    (i32.store (i32.const 0) (i32.const 1024))
    (i32.store (i32.const 4) (i32.wrap_i64 (i64.and (local.get $r) (i64.const 0xffffffff))))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
    (if (i64.ne (i64.shr_u (local.get $r) (i64.const 32)) (i64.const 0))
      (then (unreachable)))))"#;

async fn call(tool: &WasmTool, dir: &TempDir, args: Value) -> Result<String, String> {
    match tool.call(args, &dir.ctx()).await {
        Ok(output) => Ok(output.as_text()),
        Err(ToolError::ForModel(message)) => Err(message),
        Err(other) => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn module_output_becomes_the_result() {
    let dir = TempDir::new();
    let tool = WasmTool::load(config(&wat(&dir, "hallo.wat", HELLO))).unwrap();
    assert_eq!(
        (tool.spec().name.as_str(), tool.spec().effect),
        ("demo", Effect::ReadOnly)
    );
    assert_eq!(
        call(&tool, &dir, json!({})).await.unwrap(),
        "hallo aus wasm"
    );
}

#[tokio::test]
async fn endless_computation_stops_at_the_fuel_limit() {
    let dir = TempDir::new();
    let tool = WasmTool::load(config(&wat(&dir, "spin.wat", SPIN))).unwrap();
    let error = call(&tool, &dir, json!({})).await.unwrap_err();
    assert!(error.contains("Rechenbudget"), "{error}");
}

/// Abnahme Phase 6: Ohne Netzwerk-Freigabe kommt das Modul an keine Verbindung.
#[tokio::test(flavor = "multi_thread")]
async fn network_needs_a_grant() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\nconnection: close\r\n\r\nerlaubt",
                )
                .await;
        }
    });
    let dir = TempDir::new();
    let module = FETCH
        .replace("http://localhost/", &format!("http://localhost:{port}/"))
        .replace(
            "(i32.const 17)",
            &format!("(i32.const {})", format!("http://localhost:{port}/").len()),
        );
    let path = wat(&dir, "fetch.wat", &module);

    let denied = WasmTool::load(config(&path)).unwrap();
    let error = call(&denied, &dir, json!({})).await.unwrap_err();
    assert!(
        error.contains("abgestürzt"),
        "das Modul bricht ab, wenn der Abruf verweigert wird: {error}"
    );

    let granted = WasmTool::load(WasmToolConfig {
        network_hosts: vec!["localhost".into()],
        ..config(&path)
    })
    .unwrap()
    .allow_private_networks();
    assert_eq!(
        call(&granted, &dir, json!({})).await.unwrap(),
        "HTTP 200\n\nerlaubt"
    );

    let other_host = WasmTool::load(WasmToolConfig {
        network_hosts: vec!["example.com".into()],
        ..config(&path)
    })
    .unwrap()
    .allow_private_networks();
    assert!(
        call(&other_host, &dir, json!({})).await.is_err(),
        "nur freigegebene Hosts"
    );
}

#[test]
fn bad_modules_and_configs_are_refused() {
    let dir = TempDir::new();
    assert!(WasmTool::load(config(&dir.path().join("fehlt.wasm"))).is_err());
    assert!(WasmTool::load(config(&wat(&dir, "kaputt.wat", "(module (func"))).is_err());
    let parsed: WasmToolConfig =
        serde_json::from_value(json!({"name": "x", "description": "d", "module": "x.wasm"}))
            .unwrap();
    assert_eq!(
        (parsed.effect, parsed.files, parsed.memory_mb),
        (Effect::Write, FileAccess::None, 64)
    );
    assert!(
        serde_json::from_value::<WasmToolConfig>(
            json!({"name": "x", "description": "d", "module": "m", "net": true})
        )
        .is_err()
    );
}

/// Baut das Beispiel-Tool aus `wasm/demo-tool` einmal; ohne das Ziel `wasm32-wasip1` fehlt es.
fn demo_module() -> Option<PathBuf> {
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let target = root.join("target/wasm-demo");
            let status = std::process::Command::new(env!("CARGO"))
                .args([
                    "build",
                    "--release",
                    "--target",
                    "wasm32-wasip1",
                    "--manifest-path",
                ])
                .arg(root.join("wasm/demo-tool/Cargo.toml"))
                .arg("--target-dir")
                .arg(&target)
                .status();
            let module = target.join("wasm32-wasip1/release/can-demo-tool.wasm");
            if status.is_ok_and(|status| status.success()) && module.exists() {
                return Some(module);
            }
            eprintln!("Ziel wasm32-wasip1 fehlt: Tests mit dem Beispiel-Tool übersprungen");
            None
        })
        .clone()
}

#[tokio::test]
async fn demo_tool_sees_only_what_is_granted() {
    let Some(module) = demo_module() else { return };
    let dir = TempDir::new();
    dir.write("notiz.txt", b"aus dem Arbeitsverzeichnis");
    let base = config(&module);
    let tool = |config: WasmToolConfig| WasmTool::load(config).unwrap();

    let plain = tool(base.clone());
    assert_eq!(
        call(&plain, &dir, json!({"action": "echo", "text": "grüße"}))
            .await
            .unwrap(),
        "grüße"
    );
    // Dateien: ohne Freigabe nichts, mit Lesefreigabe lesen, aber nicht schreiben.
    let read = json!({"action": "read", "path": "notiz.txt"});
    assert!(
        call(&plain, &dir, read.clone())
            .await
            .unwrap_err()
            .contains("Code 2")
    );
    let reader = tool(WasmToolConfig {
        files: FileAccess::Read,
        ..base.clone()
    });
    assert_eq!(
        call(&reader, &dir, read).await.unwrap(),
        "aus dem Arbeitsverzeichnis"
    );
    let write = json!({"action": "write", "path": "neu.txt", "content": "x"});
    assert!(call(&reader, &dir, write.clone()).await.is_err());
    assert!(!dir.path().join("neu.txt").exists());
    let writer = tool(WasmToolConfig {
        files: FileAccess::ReadWrite,
        ..base.clone()
    });
    assert_eq!(call(&writer, &dir, write).await.unwrap(), "geschrieben");
    assert!(dir.path().join("neu.txt").exists());
    // Ausbruch aus /work scheitert auch mit Schreibfreigabe.
    let escape = json!({"action": "read", "path": "../../etc/passwd"});
    assert!(call(&writer, &dir, escape).await.is_err());

    // Secrets: nur freigegebene Namen.
    let mut cx = dir.ctx();
    cx.secrets = SecretsHandle::from_pairs([
        ("API_KEY", Secret::new("geheim".into())),
        ("ANDERES", Secret::new("x".into())),
    ]);
    let secrets = tool(WasmToolConfig {
        secrets: vec!["API_KEY".into()],
        ..base.clone()
    });
    let env = |name: &str| json!({"action": "env", "name": name});
    assert_eq!(
        secrets.call(env("API_KEY"), &cx).await.unwrap().as_text(),
        "geheim"
    );
    assert_eq!(
        secrets.call(env("ANDERES"), &cx).await.unwrap().as_text(),
        "(nicht gesetzt)"
    );
    assert_eq!(
        secrets.call(env("PATH"), &cx).await.unwrap().as_text(),
        "(nicht gesetzt)",
        "keine Umgebung des Hosts"
    );

    // Netzwerk ohne Freigabe: eine Meldung statt einer Verbindung.
    let fetch = json!({"action": "fetch", "url": "https://example.com/"});
    assert!(
        call(&plain, &dir, fetch)
            .await
            .unwrap_err()
            .contains("keine Hosts freigegeben")
    );

    // Speicher und Rechenzeit.
    let small = tool(WasmToolConfig {
        memory_mb: 16,
        ..base.clone()
    });
    assert_eq!(
        call(&small, &dir, json!({"action": "alloc", "mb": 4}))
            .await
            .unwrap(),
        "4 MiB belegt"
    );
    assert!(
        call(&small, &dir, json!({"action": "alloc", "mb": 64}))
            .await
            .is_err()
    );
    assert!(
        call(&plain, &dir, json!({"action": "spin"}))
            .await
            .unwrap_err()
            .contains("Rechenbudget")
    );
}
