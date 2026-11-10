//! `RuleSet`: Policy aus Regeln pro Tool-Name, Wirkung und Argumentmuster.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{Decision, ToolCall};
use crate::policy::Policy;
use crate::tool::{Effect, ToolSpec};

/// Eine Regel. Sie greift, wenn alle gesetzten Bedingungen zutreffen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// Muster für den Tool-Namen; `*` steht für beliebig viele Zeichen.
    #[serde(default = "any")]
    pub tool: String,
    /// Nur Tools dieser Wirkungsklasse.
    #[serde(default)]
    pub effect: Option<Effect>,
    /// Muster pro Argument der obersten Ebene; ein fehlendes Argument passt nie.
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    pub decision: Decision,
}

fn any() -> String {
    "*".to_owned()
}

impl Rule {
    /// Regel für Tools, deren Name auf das Muster passt. Entscheidung zunächst `Ask`.
    pub fn tool(pattern: impl Into<String>) -> Self {
        Self {
            tool: pattern.into(),
            effect: None,
            args: BTreeMap::new(),
            decision: Decision::Ask,
        }
    }

    /// Regel für alle Tools einer Wirkungsklasse. Entscheidung zunächst `Ask`.
    pub fn effect(effect: Effect) -> Self {
        Self {
            effect: Some(effect),
            ..Self::tool("*")
        }
    }

    /// Zusätzliche Bedingung: das Argument `name` passt auf `pattern`.
    pub fn arg(mut self, name: impl Into<String>, pattern: impl Into<String>) -> Self {
        self.args.insert(name.into(), pattern.into());
        self
    }

    /// Setzt die Entscheidung der Regel.
    pub fn decide(mut self, decision: Decision) -> Self {
        self.decision = decision;
        self
    }

    /// Trifft die Regel auf diesen Aufruf zu?
    pub fn matches(&self, call: &ToolCall, spec: &ToolSpec) -> bool {
        glob(&self.tool, &call.name)
            && self.effect.is_none_or(|effect| effect == spec.effect)
            && self
                .args
                .iter()
                .all(|(name, pattern)| match call.input.get(name) {
                    Some(Value::String(text)) => glob(pattern, text),
                    Some(other) => glob(pattern, &other.to_string()),
                    None => false,
                })
    }
}

/// Greifen mehrere Regeln, gewinnt die strengste. Greift keine, gilt `Ask`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// Leere Regelmenge: jeder Aufruf fragt nach.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sichere Vorgaben: lesende Tools sind erlaubt, `shell` fragt immer, alles andere fragt.
    pub fn safe_defaults() -> Self {
        Self::new()
            .rule(Rule::effect(Effect::ReadOnly).decide(Decision::Allow))
            .rule(Rule::tool("shell").decide(Decision::Ask))
    }

    /// Fügt eine Regel hinzu.
    pub fn rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }
}

impl Policy for RuleSet {
    fn check(&self, call: &ToolCall, spec: &ToolSpec) -> Decision {
        self.rules
            .iter()
            .filter(|rule| rule.matches(call, spec))
            .map(|rule| rule.decision)
            .max()
            .unwrap_or(Decision::Ask)
    }
}

/// Mustervergleich mit `*` als Platzhalter für beliebig viele Zeichen.
pub fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::CallId;
    use serde_json::json;

    fn call(name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: CallId::from_parts(1, 1),
            name: name.into(),
            input,
        }
    }

    fn spec(name: &str, effect: Effect) -> ToolSpec {
        ToolSpec::new(name, "d", json!({})).with_effect(effect)
    }

    #[test]
    fn glob_matches_literals_and_wildcards() {
        for (pattern, text, expected) in [
            ("shell", "shell", true),
            ("shell", "shell2", false),
            ("*", "", true),
            ("fs_*", "fs_read", true),
            ("fs_*", "read", false),
            ("*_file", "read_file", true),
            ("git * --stat", "git diff --stat", true),
            ("git * --stat", "git diff", false),
            ("a*b*c", "a1b2c", true),
            ("a*b*c", "a1c2b", false),
            ("ab*ab", "ab", false),
            ("", "", true),
            ("", "x", false),
        ] {
            assert_eq!(glob(pattern, text), expected, "{pattern:?} gegen {text:?}");
        }
    }

    #[test]
    fn no_matching_rule_means_ask() {
        let call = call("write_file", json!({}));
        assert_eq!(
            RuleSet::new().check(&call, &spec("write_file", Effect::Write)),
            Decision::Ask
        );
    }

    #[test]
    fn safe_defaults_allow_reads_and_always_ask_for_shell() {
        let rules = RuleSet::safe_defaults();
        assert_eq!(
            rules.check(
                &call("read_file", json!({})),
                &spec("read_file", Effect::ReadOnly)
            ),
            Decision::Allow
        );
        assert_eq!(
            rules.check(
                &call("write_file", json!({})),
                &spec("write_file", Effect::Write)
            ),
            Decision::Ask
        );
        // Selbst eine zusätzliche Erlaubnis hebt das Nachfragen für `shell` nicht auf.
        let rules = rules.rule(Rule::tool("shell").decide(Decision::Allow));
        assert_eq!(
            rules.check(&call("shell", json!({})), &spec("shell", Effect::External)),
            Decision::Ask
        );
    }

    #[test]
    fn strictest_matching_rule_wins() {
        let rules = RuleSet::new()
            .rule(Rule::tool("write_*").decide(Decision::Allow))
            .rule(
                Rule::tool("write_file")
                    .arg("path", "secrets/*")
                    .decide(Decision::Deny),
            );
        let spec = spec("write_file", Effect::Write);
        assert_eq!(
            rules.check(&call("write_file", json!({"path": "notes/a.txt"})), &spec),
            Decision::Allow
        );
        assert_eq!(
            rules.check(&call("write_file", json!({"path": "secrets/key"})), &spec),
            Decision::Deny
        );
    }

    #[test]
    fn argument_patterns_need_the_argument() {
        let rule = Rule::tool("fetch")
            .arg("port", "44*")
            .decide(Decision::Allow);
        let spec = spec("fetch", Effect::External);
        assert!(rule.matches(&call("fetch", json!({"port": 443})), &spec));
        assert!(!rule.matches(&call("fetch", json!({"port": 80})), &spec));
        assert!(!rule.matches(&call("fetch", json!({})), &spec));
    }

    #[test]
    fn rules_deserialize_with_defaults() {
        let rules: RuleSet = serde_json::from_value(json!({"rules": [
            {"effect": "read_only", "decision": "allow"},
            {"tool": "shell", "args": {"program": "rm"}, "decision": "deny"}
        ]}))
        .unwrap();
        assert_eq!(
            rules.rules[0],
            Rule::effect(Effect::ReadOnly).decide(Decision::Allow)
        );
        assert_eq!(
            rules.rules[1],
            Rule::tool("shell")
                .arg("program", "rm")
                .decide(Decision::Deny)
        );
    }
}
