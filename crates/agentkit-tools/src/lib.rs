//! Eingebaute Tools. Alle Dateizugriffe bleiben im Arbeitsverzeichnis.

pub mod fs;
pub mod read_file;
#[cfg(test)]
pub(crate) mod testkit;

pub use read_file::ReadFile;
