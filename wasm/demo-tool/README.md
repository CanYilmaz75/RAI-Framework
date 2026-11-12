# Beispiel-Tool für die WASM-Sandbox

Dieses Programm zeigt, wie ein Tool für Can als WebAssembly-Modul aussieht. Es liest die Argumente
als JSON von der Standardeingabe und schreibt das Ergebnis auf die Standardausgabe. Ein Exit-Code
ungleich null gilt als Fehler; die Fehlerausgabe geht dann ans Modell.

## Bauen

```bash
rustup target add wasm32-wasip1
```

```bash
cargo build --release --target wasm32-wasip1
```

Das Modul liegt danach unter `target/wasm32-wasip1/release/can-demo-tool.wasm`.

## Einbinden

```toml
[[wasm_tools]]
name = "demo"
description = "Liest Dateien, ruft Adressen ab und zeigt freigegebene Secrets"
module = "wasm/demo-tool/target/wasm32-wasip1/release/can-demo-tool.wasm"
effect = "read_only"
files = "read"
network_hosts = ["example.com"]
secrets = ["API_KEY"]
input_schema = { type = "object", properties = { action = { enum = ["echo", "read", "fetch", "env"] } }, required = ["action"] }
```

## Eigene Tools

- Argumente kommen als JSON auf der Standardeingabe, das Ergebnis geht auf die Standardausgabe.
- Dateien liegen, wenn freigegeben, unter `/work`.
- Netzwerk geht nur über die Host-Funktion `can.http_get(url_ptr, url_len, out_ptr, out_cap) -> i64`.
  Das Ergebnis ist `(status << 32) | geschriebene_bytes`; Status 0 heißt erfolgreich, 1 verweigert
  oder fehlgeschlagen, 2 auf den Puffer gekürzt.
- Jede Sprache, die nach `wasm32-wasip1` übersetzt, funktioniert; der Aufruf von `can.http_get`
  braucht in Rust einen `unsafe`-Block, wie in `src/main.rs`.
