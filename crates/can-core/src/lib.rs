//! Typen, Traits und Fehler des Agent-Frameworks. Kein Netzwerk.
//!
//! Alle anderen Crates hängen nur von diesem ab; `core` kennt keinen von ihnen.

pub mod assemble;
mod base64;
pub mod clock;
pub mod event;
pub mod ids;
pub mod message;
pub mod policy;
pub mod provider;
pub mod rules;
pub mod schema;
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
pub use rules::{Rule, RuleSet};
pub use secret::{Secret, SecretsHandle};
pub use state::{CallStatus, Compaction, Limits, PendingCall, Phase, RunConfig, RunState};
pub use store::{MemoryStore, Store, StoreError};
pub use tool::{Effect, ProgressSink, Tool, ToolCtx, ToolError, ToolSpec};

/// Nur für den vom `#[tool]`-Makro erzeugten Code; kein Teil der öffentlichen API.
#[doc(hidden)]
pub mod __private {
    pub use schemars;
    pub use serde_json;

    use crate::tool::ToolError;
    use serde::de::DeserializeOwned;
    use serde_json::{Map, Value};

    /// Die Argumente eines Aufrufs als JSON-Objekt.
    pub fn args_object(args: Value) -> Result<Map<String, Value>, ToolError> {
        match args {
            Value::Object(map) => Ok(map),
            Value::Null => Ok(Map::new()),
            _ => Err(ToolError::ForModel(
                "Die Argumente müssen ein JSON-Objekt sein.".into(),
            )),
        }
    }

    /// Entnimmt ein Argument und wandelt es in den Parametertyp.
    pub fn arg<T: DeserializeOwned>(
        args: &mut Map<String, Value>,
        name: &str,
    ) -> Result<T, ToolError> {
        serde_json::from_value(args.remove(name).unwrap_or(Value::Null))
            .map_err(|error| ToolError::ForModel(format!("Ungültiges Argument `{name}`: {error}")))
    }

    /// JSON-Schema eines Parametertyps.
    pub fn schema_of<T: schemars::JsonSchema>() -> Value {
        serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Bool(true))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn args_object_accepts_objects_and_null() {
            assert_eq!(args_object(json!({"a": 1})).unwrap().len(), 1);
            assert!(args_object(Value::Null).unwrap().is_empty());
            assert!(matches!(
                args_object(json!([1])),
                Err(ToolError::ForModel(_))
            ));
        }

        #[test]
        fn arg_extracts_typed_values() {
            let mut map = args_object(json!({"path": "a", "n": "x"})).unwrap();
            assert_eq!(arg::<String>(&mut map, "path").unwrap(), "a");
            assert_eq!(arg::<Option<u8>>(&mut map, "fehlt").unwrap(), None);
            let error = arg::<u8>(&mut map, "n").unwrap_err();
            assert!(error.to_string().starts_with("Ungültiges Argument `n`"));
        }

        #[test]
        fn schema_of_describes_the_type() {
            assert_eq!(schema_of::<String>()["type"], "string");
            assert_eq!(schema_of::<Vec<u32>>()["items"]["type"], "integer");
        }
    }
}
