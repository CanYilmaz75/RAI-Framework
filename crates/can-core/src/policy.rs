//! Policy-Trait: jeder Tool-Aufruf läuft hier durch, ohne Ausnahme.

use crate::event::{Decision, ToolCall};
use crate::tool::{Effect, ToolSpec};

/// Entscheidet anhand von Tool-Name, Argumenten und Wirkung, nie anhand von Modelltext.
pub trait Policy: Send + Sync {
    /// `Allow`, `Ask` oder `Deny` für diesen Aufruf.
    fn check(&self, call: &ToolCall, spec: &ToolSpec) -> Decision;
}

/// Erlaubt jeden Aufruf. Nur für Tests und vertrauenswürdige Umgebungen.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl Policy for AllowAll {
    fn check(&self, _call: &ToolCall, _spec: &ToolSpec) -> Decision {
        Decision::Allow
    }
}

/// Lehnt jeden Aufruf ab.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAll;

impl Policy for DenyAll {
    fn check(&self, _call: &ToolCall, _spec: &ToolSpec) -> Decision {
        Decision::Deny
    }
}

/// Sichere Vorgabe: lesende Tools sind erlaubt, alles andere fragt nach.
#[derive(Clone, Copy, Debug, Default)]
pub struct ByEffect;

impl Policy for ByEffect {
    fn check(&self, _call: &ToolCall, spec: &ToolSpec) -> Decision {
        match spec.effect {
            Effect::ReadOnly => Decision::Allow,
            Effect::Write | Effect::External => Decision::Ask,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::CallId;
    use serde_json::json;

    fn call() -> ToolCall {
        ToolCall {
            id: CallId::from_parts(1, 1),
            name: "t".into(),
            input: json!({}),
        }
    }

    fn spec(effect: Effect) -> ToolSpec {
        ToolSpec::new("t", "d", json!({})).with_effect(effect)
    }

    #[test]
    fn allow_all_and_deny_all() {
        assert_eq!(
            AllowAll.check(&call(), &spec(Effect::Write)),
            Decision::Allow
        );
        assert_eq!(
            DenyAll.check(&call(), &spec(Effect::ReadOnly)),
            Decision::Deny
        );
    }

    #[test]
    fn by_effect_asks_for_everything_but_reads() {
        assert_eq!(
            ByEffect.check(&call(), &spec(Effect::ReadOnly)),
            Decision::Allow
        );
        assert_eq!(ByEffect.check(&call(), &spec(Effect::Write)), Decision::Ask);
        assert_eq!(
            ByEffect.check(&call(), &spec(Effect::External)),
            Decision::Ask
        );
    }
}
