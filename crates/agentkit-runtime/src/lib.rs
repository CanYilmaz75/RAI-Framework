//! Agent-Loop als Zustandsmaschine: eine reine Schrittfunktion entscheidet, ein Treiber führt aus.

pub mod agent;
pub mod clock;
pub mod context;
mod driver;
mod exec;
pub mod registry;
pub mod run;
pub mod step;

pub use agent::{Agent, AgentBuilder, BuildError};
pub use clock::SystemClock;
pub use context::build_request;
pub use registry::Registry;
pub use run::{Outcome, Run, RunError};
pub use step::{Effect, Input, Step, step};
