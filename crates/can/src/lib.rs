//! Can: ein AI-Agent-Framework. Dieses Crate ist die Fassade über die einzelnen Crates und
//! der Teil mit Stabilitätsversprechen; die internen Crates bleiben austauschbar.
//!
//! Ohne Feature-Flags baut nur der Kern mit `MemoryStore` und `FakeProvider`. Die Flags
//! `openai`, `anthropic`, `gemini`, `vertex`, `bedrock` (oder alle mit `providers`), `sqlite`,
//! `postgres`, `http`, `mcp` und `wasm` schalten Anbieter, Stores und Tools dazu.
//!
//! ```
//! use can::prelude::*;
//!
//! /// Zählt die Wörter eines Textes.
//! #[tool(effect = "read_only", idempotent)]
//! async fn count_words(text: String) -> Result<String, ToolError> {
//!     Ok(text.split_whitespace().count().to_string())
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let provider = FakeProvider::new()
//!     .tool_call("count_words", serde_json::json!({"text": "eins zwei drei"}))
//!     .text("Es sind drei Wörter.");
//! let agent = Agent::builder().provider(provider).model("fake").tool(count_words).build()?;
//! let outcome = agent.complete(SessionId::new(), "Wie viele Wörter?").await?;
//! assert_eq!(outcome.text, "Es sind drei Wörter.");
//! # Ok(())
//! # }
//! ```

pub use can_core as core;
pub use can_macros::tool;
pub use can_providers as providers;
pub use can_runtime as runtime;
pub use can_store as store;
pub use can_tools as tools;

/// Alles, was ein typisches Programm braucht, mit einem `use can::prelude::*;`.
pub mod prelude {
    pub use can_core::{
        AllowAll, BlockList, ByEffect, Clock, Decision, DenyAll, Effect, Envelope, Event,
        FinishReason, Limits, MemoryStore, Message, Moderator, Policy, Pricing, Provider, Rule,
        RuleSet, Secret, SecretsHandle, SessionId, Store, Tool, ToolCtx, ToolError, ToolOutput,
        ToolSpec,
    };
    pub use can_macros::tool;
    pub use can_providers::{FakeProvider, Fallback, Recording, Retry};
    pub use can_runtime::{Agent, AgentBuilder, Outcome, Run, RunError, SubAgent, SystemClock};
    pub use can_tools::{ReadFile, Sandbox, SandboxedShell, shell, write_file};

    #[cfg(feature = "anthropic")]
    pub use can_providers::Anthropic;
    #[cfg(feature = "bedrock")]
    pub use can_providers::Bedrock;
    #[cfg(feature = "gemini")]
    pub use can_providers::Gemini;
    #[cfg(feature = "openai")]
    pub use can_providers::OpenAiCompat;
    #[cfg(feature = "providers")]
    pub use can_providers::ProviderSettings;
    #[cfg(feature = "vertex")]
    pub use can_providers::VertexAnthropic;
    #[cfg(feature = "postgres")]
    pub use can_store::PostgresStore;
    #[cfg(feature = "sqlite")]
    pub use can_store::SqliteStore;
    #[cfg(feature = "http")]
    pub use can_tools::HttpFetch;
    #[cfg(feature = "mcp")]
    pub use can_tools::McpClient;
    #[cfg(feature = "wasm")]
    pub use can_tools::{FileAccess, WasmTool, WasmToolConfig};
}
