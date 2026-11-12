//! Agent-Loop als Zustandsmaschine: eine reine Schrittfunktion entscheidet, ein Treiber führt aus.

pub mod agent;
pub mod clock;
pub mod context;
mod driver;
mod driver_input;
mod driver_model;
mod exec;
mod fork;
pub mod redact;
pub mod registry;
pub mod run;
pub mod step;
mod step_plan;
mod step_types;
mod structured;
pub mod subagent;

pub use agent::{Agent, AgentBuilder, BuildError};
pub use clock::SystemClock;
pub use context::{build_request, compaction_due, estimate_tokens, plan_compaction};
pub use redact::Redactor;
pub use registry::Registry;
pub use run::{Outcome, Run, RunController, RunError};
pub use step::{Effect, Input, Step, step};
pub use subagent::SubAgent;
