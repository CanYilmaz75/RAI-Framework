//! Modellanbieter hinter Feature-Flags, plus Fake-Provider für Tests.
//!
//! - `openai`: jede OpenAI-kompatible Schnittstelle, darunter Azure OpenAI, Ollama, OpenRouter,
//!   Groq, Mistral, DeepSeek, xAI, Together, Fireworks und Perplexity
//! - `anthropic`: Anthropic API
//! - `gemini`: Gemini API und Gemini auf Vertex AI
//! - `vertex`: zusätzlich Claude auf Vertex AI
//! - `bedrock`: Amazon Bedrock über die Converse-API
//! - `all`: alles davon samt [`settings::ProviderSettings`]

#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(any(feature = "gemini", feature = "vertex"))]
pub mod auth;
#[cfg(feature = "bedrock")]
pub mod bedrock;
pub mod fake;
pub mod fallback;
#[cfg(feature = "gemini")]
pub mod gemini;
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "gemini",
    feature = "bedrock"
))]
mod http;
#[cfg(feature = "openai")]
pub mod openai;
pub mod presets;
pub mod recording;
pub mod retry;
#[cfg(feature = "all")]
pub mod settings;
pub mod sse;
#[cfg(feature = "vertex")]
pub mod vertex;

#[cfg(feature = "anthropic")]
pub use anthropic::Anthropic;
#[cfg(feature = "bedrock")]
pub use bedrock::Bedrock;
pub use fake::FakeProvider;
pub use fallback::Fallback;
#[cfg(feature = "gemini")]
pub use gemini::Gemini;
#[cfg(feature = "openai")]
pub use openai::OpenAiCompat;
pub use recording::Recording;
pub use retry::Retry;
#[cfg(feature = "all")]
pub use settings::ProviderSettings;
#[cfg(feature = "vertex")]
pub use vertex::VertexAnthropic;
