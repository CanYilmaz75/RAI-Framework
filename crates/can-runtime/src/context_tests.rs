use super::*;
use can_core::{CallId, Digest, Event, RunConfig, StopReason, ToolSpec, Usage};
use serde_json::json;

/// Zustand mit `rounds` Runden aus Modellantwort, Tool-Aufruf und Ergebnis der Größe `size`.
fn state(rounds: u128, size: usize, window: u32) -> RunState {
    let config = RunConfig {
        model: "m".into(),
        system: Some("sys".into()),
        tools: vec![
            ToolSpec::new("zeta", "z", json!({})),
            ToolSpec::new("alpha", "a", json!({})),
        ],
        max_output: Some(100),
        context_window: window,
        reserve_output: 100,
        ..RunConfig::default()
    };
    let mut state = RunState::new(config);
    state.apply(&Event::RunStarted {
        input: Message::user("Aufgabe"),
        config_digest: Digest::of(b""),
    });
    for round in 1..=rounds {
        let id = CallId::from_parts(1, round);
        let call = ContentBlock::ToolUse {
            id,
            name: "alpha".into(),
            input: json!({"n": round as u64}),
        };
        state.apply(&Event::ModelRequested { step: round as u32 });
        state.apply(&Event::ModelResponded {
            step: round as u32,
            message: Message {
                role: Role::Assistant,
                content: vec![call.clone()],
            },
            usage: Usage::default(),
            stop: StopReason::ToolUse,
        });
        let ContentBlock::ToolUse { id, name, input } = call else {
            unreachable!()
        };
        state.apply(&Event::ToolRequested {
            call: can_core::ToolCall { id, name, input },
        });
        let output = ToolOutput::text(format!("Runde {round}: {}", "x".repeat(size)));
        state.apply(&Event::ToolFinished {
            call_id: id,
            output,
            is_error: false,
        });
    }
    state
}

#[test]
fn request_has_sorted_tools_and_full_history() {
    let state = state(1, 10, 0);
    let request = build_request(&state);
    let names: Vec<&str> = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, vec!["alpha", "zeta"]);
    assert_eq!(request.messages.len(), 3);
    assert_eq!(
        (request.model.as_str(), request.system.as_deref()),
        ("m", Some("sys"))
    );
    assert_eq!(request.max_output, Some(100));
    assert_eq!(budget(&state), None, "unbekanntes Fenster: nie verdichten");
    assert!(compaction_due(&state).is_none());
}

#[test]
fn estimate_and_budget() {
    let small = state(1, 10, 1_100);
    assert_eq!(budget(&small), Some(800));
    assert_eq!(
        estimate_tokens(&Request::default()),
        (serde_json::to_string(&Request::default()).unwrap().len() / 4) as u64
    );
    assert!(estimate_tokens(&build_request(&state(3, 4_000, 0))) > 3_000);
}

#[test]
fn pre_stage_shortens_old_outputs_before_any_summary() {
    // Zehn Runden mit je 3 000 Bytes: ungekürzt rund 7 500 Token, mit Vorschau deutlich weniger.
    let state = state(10, 3_000, 8_000);
    let request = build_request(&state);
    assert!(estimate_tokens(&request) < 6_000);
    let texts: Vec<String> = request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult { output, .. } => Some(output.as_text()),
            _ => None,
        })
        .collect();
    assert!(texts[0].contains("alte Ausgabe gekürzt") && texts[0].len() < 500);
    assert_eq!(
        texts[9].len(),
        "Runde 10: ".len() + 3_000,
        "die letzten Runden bleiben wörtlich"
    );
    assert!(
        compaction_due(&state).is_none(),
        "die Vorstufe reicht hier aus"
    );
}

#[test]
fn compaction_cuts_between_rounds_and_keeps_the_last_four() {
    let state = state(10, 3_000, 2_000);
    let (up_to_seq, request) = compaction_due(&state).unwrap();
    // Sechs Runden fallen weg; die siebte Modellantwort ist die erste, die bleibt.
    let cut = state
        .message_seqs
        .iter()
        .position(|seq| *seq > up_to_seq)
        .unwrap();
    assert_eq!(state.messages[cut].role, Role::Assistant);
    assert_eq!(state.messages[cut - 1].role, Role::Tool);
    assert_eq!(state.messages.len() - cut, KEEP_ROUNDS * 2);
    assert!(request.tools.is_empty());
    let transcript = request.messages[0].text();
    assert!(
        transcript.contains("Nutzer: Aufgabe") && transcript.contains("Assistent ruft alpha auf")
    );
    assert!(transcript.contains("Runde 6") && !transcript.contains("Runde 7"));
}

#[test]
fn after_compaction_the_model_sees_task_summary_and_recent_rounds() {
    let mut state = state(10, 3_000, 2_000);
    let (up_to_seq, _) = compaction_due(&state).unwrap();
    state.apply(&Event::Compacted {
        up_to_seq,
        summary: Message::assistant("Sechs Runden erledigt."),
    });
    let request = build_request(&state);
    assert_eq!(request.messages.len(), 2 + KEEP_ROUNDS * 2);
    assert_eq!(request.messages[0], Message::user("Aufgabe"));
    assert_eq!(
        request.messages[1].text(),
        "Zusammenfassung des bisherigen Verlaufs:\nSechs Runden erledigt."
    );
    assert_eq!(request.messages[2].role, Role::Assistant);
    // Jeder Aufruf hat sein Ergebnis direkt dahinter.
    for pair in request.messages[2..].chunks(2) {
        let ContentBlock::ToolUse { id, .. } = &pair[0].content[0] else {
            panic!("kein Aufruf")
        };
        assert!(
            matches!(&pair[1].content[0], ContentBlock::ToolResult { call_id, .. } if call_id == id)
        );
    }
    assert!(
        plan_compaction(&state, KEEP_ROUNDS).is_none(),
        "nichts mehr zusammenzufassen"
    );
    let (next, request) = plan_compaction(&state, 1).unwrap();
    assert!(next > up_to_seq);
    assert!(
        request.messages[0]
            .text()
            .starts_with("Fasse diesen Verlauf zusammen:\n\nFrühere Zusammenfassung:")
    );
}

#[test]
fn nothing_to_compact_with_few_rounds() {
    assert!(plan_compaction(&state(4, 10, 100), KEEP_ROUNDS).is_none());
    assert!(plan_compaction(&state(0, 10, 100), 1).is_none());
    assert!(plan_compaction(&state(5, 10, 100), KEEP_ROUNDS).is_some());
}
