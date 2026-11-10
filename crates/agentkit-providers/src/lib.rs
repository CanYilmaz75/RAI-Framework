//! Modellanbieter hinter Feature-Flags, plus Fake-Provider für Tests.

pub mod fake;
#[cfg(feature = "openai")]
pub mod openai;
pub mod sse;

pub use fake::FakeProvider;
#[cfg(feature = "openai")]
pub use openai::OpenAiCompat;
