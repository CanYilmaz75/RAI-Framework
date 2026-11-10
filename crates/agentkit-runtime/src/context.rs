//! Baut aus dem Zustand die Anfrage, die das Modell sieht.

use agentkit_core::{Request, RunState, ToolDecl};

/// Anfrage aus Konfiguration und Verlauf. Tools stehen alphabetisch sortiert, damit der Anfang
/// der Anfrage zwischen Schritten gleich bleibt und ein Prompt-Cache ihn wiederverwenden kann.
pub fn build_request(state: &RunState) -> Request {
    let mut tools: Vec<ToolDecl> = state.config.tools.iter().map(|spec| spec.decl()).collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Request {
        model: state.config.model.clone(),
        system: state.config.system.clone(),
        messages: state.messages.clone(),
        tools,
        max_output: state.config.max_output,
        extra: state.config.extra.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{Digest, Event, Message, RunConfig, ToolSpec};
    use serde_json::json;

    #[test]
    fn request_has_sorted_tools_and_full_history() {
        let config = RunConfig {
            model: "m".into(),
            system: Some("sys".into()),
            tools: vec![
                ToolSpec::new("zeta", "z", json!({})),
                ToolSpec::new("alpha", "a", json!({})),
            ],
            max_output: Some(100),
            ..RunConfig::default()
        };
        let mut state = RunState::new(config);
        state.apply(&Event::RunStarted {
            input: Message::user("hi"),
            config_digest: Digest::of(b""),
        });
        let request = build_request(&state);
        let names: Vec<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(request.messages, vec![Message::user("hi")]);
        assert_eq!(
            (request.model.as_str(), request.system.as_deref()),
            ("m", Some("sys"))
        );
        assert_eq!(request.max_output, Some(100));
    }
}
