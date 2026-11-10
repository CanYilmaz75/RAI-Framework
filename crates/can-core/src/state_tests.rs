use super::*;
use crate::event::{Approver, Decision, LimitKind, SCHEMA_VERSION};
use crate::message::StopReason;
use serde_json::json;

fn call(n: u128) -> ToolCall {
    ToolCall {
        id: CallId::from_parts(1, n),
        name: "read_file".into(),
        input: json!({"path": "a"}),
    }
}

fn responded(calls: &[ToolCall]) -> Event {
    let content = calls
        .iter()
        .map(|c| ContentBlock::ToolUse {
            id: c.id,
            name: c.name.clone(),
            input: c.input.clone(),
        })
        .collect();
    Event::ModelResponded {
        step: 1,
        message: Message {
            role: Role::Assistant,
            content,
        },
        usage: Usage {
            input_tokens: 5,
            output_tokens: 2,
            cached_tokens: 0,
        },
        stop: StopReason::ToolUse,
    }
}

fn started() -> RunState {
    let mut state = RunState::new(RunConfig::default());
    state.apply(&Event::RunStarted {
        input: Message::user("hi"),
        config_digest: Digest::of(b""),
    });
    state.apply(&Event::ModelRequested { step: 1 });
    state
}

fn finished(c: &ToolCall, text: &str) -> Event {
    Event::ToolFinished {
        call_id: c.id,
        output: ToolOutput::text(text),
        is_error: false,
    }
}

fn allow(c: &ToolCall) -> Event {
    Event::ApprovalResolved {
        call_id: c.id,
        decision: Decision::Allow,
        by: Approver::Rule,
    }
}

#[test]
fn run_started_and_model_requested() {
    let state = started();
    assert_eq!(state.phase, Phase::AwaitModel);
    assert_eq!(state.step, 1);
    assert_eq!(state.messages, vec![Message::user("hi")]);
}

#[test]
fn plain_answer_finishes() {
    let mut state = started();
    state.apply(&Event::ModelResponded {
        step: 1,
        message: Message::assistant("fertig"),
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
        },
        stop: StopReason::EndTurn,
    });
    state.apply(&Event::RunFinished {
        reason: FinishReason::Completed,
    });
    assert_eq!(state.phase, Phase::Done);
    assert_eq!(state.finish, Some(FinishReason::Completed));
    assert_eq!(state.last_assistant_text(), "fertig");
    assert_eq!(state.usage.total(), 2);
}

#[test]
fn tool_round_moves_through_policy_and_tools() {
    let (a, b) = (call(1), call(2));
    let mut state = started();
    state.apply(&responded(&[a.clone(), b.clone()]));
    state.apply(&Event::ToolRequested { call: a.clone() });
    state.apply(&Event::ToolRequested { call: b.clone() });
    assert_eq!(state.phase, Phase::AwaitPolicy);

    state.apply(&Event::ApprovalNeeded { call_id: b.id });
    assert_eq!(
        state.pending_call(b.id).unwrap().status,
        CallStatus::AwaitingApproval
    );
    state.apply(&allow(&a));
    assert_eq!(state.phase, Phase::AwaitPolicy, "b ist noch offen");
    state.apply(&allow(&b));
    assert_eq!(state.phase, Phase::RunTools);

    state.apply(&Event::ToolStarted { call_id: a.id });
    assert_eq!(
        state.pending_call(a.id).unwrap().status,
        CallStatus::Started
    );
    // Ergebnisse treffen in umgekehrter Reihenfolge ein.
    state.apply(&Event::ToolStarted { call_id: b.id });
    state.apply(&finished(&b, "B"));
    assert_eq!(state.messages.len(), 2, "noch keine Ergebnisnachricht");
    state.apply(&finished(&a, "A"));

    assert!(state.pending.is_empty());
    assert_eq!(state.phase, Phase::RunTools);
    let results = state.messages.last().unwrap();
    assert_eq!(results.role, Role::Tool);
    let ids: Vec<CallId> = results
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { call_id, .. } => Some(*call_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec![a.id, b.id],
        "Reihenfolge der Aufrufe, nicht der Fertigstellung"
    );
}

#[test]
fn denied_call_never_starts_but_gets_a_result() {
    let a = call(1);
    let mut state = started();
    state.apply(&responded(std::slice::from_ref(&a)));
    state.apply(&Event::ToolRequested { call: a.clone() });
    state.apply(&Event::ApprovalResolved {
        call_id: a.id,
        decision: Decision::Deny,
        by: Approver::Human,
    });
    assert_eq!(state.pending_call(a.id).unwrap().status, CallStatus::Denied);
    state.apply(&Event::ToolFinished {
        call_id: a.id,
        output: ToolOutput::text("abgelehnt"),
        is_error: true,
    });
    assert!(state.pending.is_empty());
    assert!(matches!(
        state.messages.last().unwrap().content[0],
        ContentBlock::ToolResult { is_error: true, .. }
    ));
}

#[test]
fn finishing_mid_tools_closes_open_calls() {
    let a = call(1);
    let mut state = started();
    state.apply(&responded(std::slice::from_ref(&a)));
    state.apply(&Event::ToolRequested { call: a.clone() });
    state.apply(&Event::RunFinished {
        reason: FinishReason::Cancelled,
    });
    assert_eq!(state.phase, Phase::Done);
    assert!(state.pending.is_empty());
    assert_eq!(state.messages.last().unwrap().role, Role::Tool);
}

#[test]
fn second_run_keeps_history_and_resets_counters() {
    let mut state = started();
    state.apply(&Event::ModelResponded {
        step: 1,
        message: Message::assistant("eins"),
        usage: Usage {
            input_tokens: 9,
            output_tokens: 9,
            cached_tokens: 0,
        },
        stop: StopReason::EndTurn,
    });
    state.apply(&Event::RunFinished {
        reason: FinishReason::LimitReached {
            limit: LimitKind::Tokens,
        },
    });
    state.apply(&Event::RunStarted {
        input: Message::user("zwei"),
        config_digest: Digest::of(b""),
    });
    assert_eq!(state.messages.len(), 3);
    assert_eq!(
        (state.step, state.usage.total(), state.finish.clone()),
        (0, 0, None)
    );
    assert_eq!(state.phase, Phase::AwaitModel);
}

#[test]
fn envelope_sets_seq_run_and_deadline() {
    let run_id = RunId::from_parts(3, 3);
    let log = vec![
        Envelope {
            seq: 0,
            run_id,
            ts: Timestamp(1_000),
            schema: SCHEMA_VERSION,
            event: Event::RunStarted {
                input: Message::user("hi"),
                config_digest: Digest::of(b""),
            },
        },
        Envelope {
            seq: 1,
            run_id,
            ts: Timestamp(1_001),
            schema: SCHEMA_VERSION,
            event: Event::ModelRequested { step: 1 },
        },
    ];
    let state = RunState::fold(RunConfig::default(), &log);
    assert_eq!(state.next_seq, 2);
    assert_eq!(state.run_id, Some(run_id));
    assert_eq!(state.deadline(), Some(Timestamp(601_000)));
}

#[test]
fn volatile_and_unknown_ids_change_nothing() {
    let mut state = started();
    let before = state.clone();
    state.apply(&Event::TextDelta {
        step: 1,
        text: "x".into(),
    });
    state.apply(&Event::ToolStarted {
        call_id: CallId::from_parts(9, 9),
    });
    state.phase = before.phase;
    assert_eq!(state, before);
}

#[test]
fn state_and_config_roundtrip() {
    let a = call(1);
    let mut state = started();
    state.apply(&responded(std::slice::from_ref(&a)));
    state.apply(&Event::ToolRequested { call: a });
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(serde_json::from_str::<RunState>(&json).unwrap(), state);
}

#[test]
fn config_digest_and_tool_lookup() {
    let mut config = RunConfig::default();
    let empty = config.digest();
    config
        .tools
        .push(ToolSpec::new("read_file", "liest", json!({})));
    assert_ne!(config.digest(), empty);
    assert_eq!(config.digest(), config.clone().digest());
    assert!(config.tool("read_file").is_some());
    assert!(config.tool("fehlt").is_none());
    assert_eq!(Limits::default().max_steps, 50);
}
