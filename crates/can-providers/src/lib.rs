//! Modellanbieter hinter Feature-Flags, plus Fake-Provider für Tests.

#[cfg(feature = "anthropic")]
pub mod anthropic;
pub mod fake;
pub mod fallback;
#[cfg(any(feature = "openai", feature = "anthropic"))]
mod http;
#[cfg(feature = "openai")]
pub mod openai;
pub mod recording;
pub mod retry;
pub mod sse;

#[cfg(feature = "anthropic")]
pub use anthropic::Anthropic;
pub use fake::FakeProvider;
pub use fallback::Fallback;
#[cfg(feature = "openai")]
pub use openai::OpenAiCompat;
pub use recording::Recording;
pub use retry::Retry;
