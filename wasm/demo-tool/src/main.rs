//! Beispiel-Tool für die WASM-Sandbox von Can.
//!
//! Die Argumente kommen als JSON auf der Standardeingabe, das Ergebnis geht auf die
//! Standardausgabe. Das Feld `action` wählt, was das Tool tut; so lässt sich an einem Modul zeigen,
//! welche Freigaben wirken.
//!
//! ```text
//! cargo build --release --target wasm32-wasip1
//! ```

use std::io::Read;

#[link(wasm_import_module = "can")]
unsafe extern "C" {
    /// Vom Host bereitgestellter HTTP-Abruf; siehe `can-tools/src/wasm.rs`.
    fn http_get(url_ptr: *const u8, url_len: usize, out_ptr: *mut u8, out_cap: usize) -> i64;
}

/// Ruft eine Adresse über den Host ab. Ohne Freigabe kommt eine Fehlermeldung zurück.
fn fetch(url: &str) -> Result<String, String> {
    let mut buffer = vec![0u8; 64 * 1024];
    // SAFETY: Zeiger und Längen stammen aus gültigen, lebenden Puffern dieses Moduls.
    let result = unsafe { http_get(url.as_ptr(), url.len(), buffer.as_mut_ptr(), buffer.len()) };
    let (status, written) = (result >> 32, (result & 0xffff_ffff) as usize);
    let text = String::from_utf8_lossy(&buffer[..written]).into_owned();
    if status == 1 { Err(text) } else { Ok(text) }
}

fn run() -> Result<String, String> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).map_err(|error| error.to_string())?;
    let args: serde_json::Value = serde_json::from_str(&input).map_err(|error| format!("kein JSON: {error}"))?;
    let text = |key: &str| args[key].as_str().unwrap_or_default().to_owned();
    match args["action"].as_str().unwrap_or_default() {
        "echo" => Ok(text("text")),
        "read" => std::fs::read_to_string(format!("/work/{}", text("path"))).map_err(|error| error.to_string()),
        "write" => std::fs::write(format!("/work/{}", text("path")), text("content"))
            .map(|()| "geschrieben".to_owned())
            .map_err(|error| error.to_string()),
        "env" => Ok(std::env::var(text("name")).unwrap_or_else(|_| "(nicht gesetzt)".to_owned())),
        "fetch" => fetch(&text("url")),
        "alloc" => {
            let megabytes = args["mb"].as_u64().unwrap_or(1) as usize;
            let block = vec![1u8; megabytes * 1024 * 1024];
            Ok(format!("{} MiB belegt", block.len() / 1024 / 1024))
        }
        "spin" => {
            let mut counter: u64 = 0;
            loop {
                counter = std::hint::black_box(counter.wrapping_add(1));
            }
        }
        other => Err(format!("unbekannte Aktion: {other}")),
    }
}

fn main() {
    match run() {
        Ok(text) => print!("{text}"),
        Err(error) => {
            eprint!("{error}");
            std::process::exit(2);
        }
    }
}
