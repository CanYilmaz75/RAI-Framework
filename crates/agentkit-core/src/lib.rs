//! Typen, Traits und Fehler des Agent-Frameworks. Kein Netzwerk.
//!
//! Alle anderen Crates hängen nur von diesem ab; `core` kennt keinen von ihnen.

pub mod assemble;
pub mod clock;
pub mod event;
pub mod ids;
pub mod message;
pub mod policy;
pub mod provider;
pub mod secret;
pub mod state;
pub mod store;
pub mod tool;

pub use assemble::Assembler;
pub use clock::{Clock, ManualClock};
pub use event::{
    Approver, Decision, Digest, Envelope, Event, FinishReason, LimitKind, SCHEMA_VERSION,
    Timestamp, ToolCall,
};
pub use ids::{CallId, IdError, RunId, SessionId};
pub use message::{Bytes, ContentBlock, Message, OutputBlock, Role, StopReason, ToolOutput, Usage};
pub use policy::{AllowAll, ByEffect, DenyAll, Policy};
pub use provider::{
    BoxError, Capabilities, Chunk, Provider, ProviderError, ProviderStream, Request, Schema,
    ToolDecl,
};
pub use secret::{Secret, SecretsHandle};
pub use state::{CallStatus, Limits, PendingCall, Phase, RunConfig, RunState};
pub use store::{MemoryStore, Store, StoreError};
pub use tool::{Effect, ProgressSink, Tool, ToolCtx, ToolError, ToolSpec};
