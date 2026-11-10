//! Eingebaute Tools. Alle Dateizugriffe bleiben im Arbeitsverzeichnis.

pub mod fs;
pub mod read_file;
mod shell_tool;
#[cfg(test)]
pub(crate) mod testkit;
mod write_tool;

pub use read_file::ReadFile;
pub use shell_tool::shell;
pub use write_tool::write_file;
