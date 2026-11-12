//! Eingebaute Tools. Alle Dateizugriffe bleiben im Arbeitsverzeichnis.

pub mod fs;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod read_file;
pub mod sandbox;
mod shell_tool;
#[cfg(test)]
pub(crate) mod testkit;
mod write_tool;

#[cfg(feature = "http")]
pub use http::HttpFetch;
#[cfg(feature = "mcp")]
pub use mcp::{McpClient, McpError};
pub use read_file::ReadFile;
pub use sandbox::{Sandbox, SandboxMode};
pub use shell_tool::{SandboxedShell, shell};
pub use write_tool::write_file;
