//! Attribut-Makro `#[tool]`: macht aus einer `async fn` ein Tool.
//!
//! Name und Beschreibung kommen aus Funktionsname und Doc-Kommentar, das JSON-Schema aus den
//! Parametertypen (über `schemars`). Das Makro bleibt dünn: der handgeschriebene `Tool`-Trait
//! ist immer gleichwertig.
//!
//! ```ignore
//! /// Liest eine Textdatei im Arbeitsverzeichnis.
//! #[tool(effect = "read_only", idempotent)]
//! async fn read_file(cx: &ToolCtx, path: String) -> Result<String, ToolError> {
//!     fs::read_text(cx, &path).await
//! }
//! ```
//!
//! Attribute: `effect = "read_only" | "write" | "external"` (Vorgabe `write`), `idempotent`
//! (Vorgabe: nicht idempotent), `timeout = <Sekunden>` (Vorgabe 60) und `name = "..."`.
//! Der erste Parameter darf eine Referenz auf `ToolCtx` sein; alle weiteren sind Argumente
//! des Modells. Ein `Option<T>` ist kein Pflichtfeld.

mod expand;

use proc_macro::TokenStream;

/// Siehe die Beschreibung des Crates.
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand::expand(attr.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
