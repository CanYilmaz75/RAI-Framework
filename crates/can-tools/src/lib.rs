//! Eingebaute Tools. Alle Dateizugriffe bleiben im Arbeitsverzeichnis.

mod edit;
pub mod fs;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod read_file;
pub mod sandbox;
pub mod search;
mod shell_tool;
pub mod skills;
#[cfg(test)]
pub(crate) mod testkit;
pub mod todos;
mod write_tool;

pub use edit::edit_file;
#[cfg(feature = "http")]
pub use http::HttpFetch;
#[cfg(feature = "mcp")]
pub use mcp::{McpClient, McpError};
pub use read_file::ReadFile;
pub use sandbox::{Sandbox, SandboxMode};
pub use search::{glob, grep, ls};
pub use shell_tool::{SandboxedShell, shell};
pub use skills::{LoadSkill, SkillSet, load_memory, system_prompt};
pub use todos::WriteTodos;
pub use write_tool::write_file;
