//! Tabellen-Tests: jeder Übergang der Zustandsmaschine einzeln.

use super::*;
use can_core::{ContentBlock, Limits, Role, RunConfig, ToolSpec};
use serde_json::json;
use std::time::Duration;

pub(super) fn config() -> RunConfig {
    RunConfig {
        model: "m".into(),
        tools: vec![
            ToolSpec::new("read", "liest", json!({})).with_idempotent(true),
            ToolSpec::new("write", "schreibt", json!({})),
        ],
        limits: Limits {
            max_steps: 3,
            max_tokens: 1_000,
            timeout: Duration::from_secs(60),
            max_tool_calls_per_step: 3,
        },
        ..RunConfig::default()
    }
}

pub(super) fn call(n: u128, name: &str) -> ToolCall {
    ToolCall {
        id: CallId::from_parts(1, n),
        name: name.into(),
        input: json!({}),
    }
}

pub(super) fn model_done(calls: &[ToolCall], tokens: u64) -> Input {
    let mut content = vec![ContentBlock::Text("text".into())];
    for c in calls {
        content.push(ContentBlock::ToolUse {
            id: c.id,
            name: c.name.clone(),
            input: c.input.clone(),
        });
    }
    Input::ModelDone {
        message: Message {
            role: Role::Assistant,
            content,
        },
        usage: Usage {
            input_tokens: tokens,
            output_tokens: 0,
            cached_tokens: 0,
        },
        stop: if calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        },
    }
}

/// Führt einen Schritt aus, wendet seine Events an und gibt ihn zurück.
pub(super) fn drive(state: &mut RunState, input: Input) -> Step {
    let step = step(state, input);
    for event in &step.events {
        state.apply(event);
    }
    step
}

pub(super) fn kinds(step: &Step) -> Vec<&'static str> {
    step.events.iter().map(Event::kind).collect()
}

pub(super) fn effects(step: &Step) -> Vec<&'static str> {
    step.effects
        .iter()
        .map(|effect| match effect {
            Effect::CallModel(_) => "call_model",
            Effect::CheckPolicy(_) => "check_policy",
            Effect::RunTool(_) => "run_tool",
        })
        .collect()
}

pub(super) fn decide(c: &ToolCall, decision: Decision) -> Input {
    Input::PolicyDecided {
        call_id: c.id,
        decision,
        by: Approver::Rule,
    }
}

pub(super) fn done(c: &ToolCall) -> Input {
    Input::ToolDone {
        call_id: c.id,
        output: ToolOutput::text("ok"),
        is_error: false,
    }
}

/// Zustand in `AwaitModel` nach dem Start.
pub(super) fn awaiting_model() -> RunState {
    let mut state = RunState::new(config());
    drive(&mut state, Input::Start(Message::user("hi")));
    state
}

/// Zustand in `AwaitPolicy` mit den angegebenen Aufrufen.
pub(super) fn awaiting_policy(calls: &[ToolCall]) -> RunState {
    let mut state = awaiting_model();
    drive(&mut state, model_done(calls, 1));
    state
}

#[test]
fn idle_start_requests_the_model() {
    let mut state = RunState::new(config());
    let step = drive(&mut state, Input::Start(Message::user("hi")));
    assert_eq!(kinds(&step), ["run_started", "model_requested"]);
    assert_eq!(effects(&step), ["call_model"]);
    assert_eq!(state.phase, Phase::AwaitModel);
    let Effect::CallModel(request) = &step.effects[0] else {
        panic!("kein CallModel")
    };
    assert_eq!(
        request.messages,
        vec![Message::user("hi")],
        "die Anfrage enthält die Eingabe"
    );
}

#[test]
fn model_done_without_tools_completes() {
    let mut state = awaiting_model();
    let step = drive(&mut state, model_done(&[], 1));
    assert_eq!(kinds(&step), ["model_responded", "run_finished"]);
    assert!(step.effects.is_empty());
    assert_eq!(
        (state.phase, state.finish.clone()),
        (Phase::Done, Some(FinishReason::Completed))
    );
}

#[test]
fn model_done_with_tools_checks_policy_per_call() {
    let mut state = awaiting_model();
    let step = drive(
        &mut state,
        model_done(&[call(1, "read"), call(2, "write")], 1),
    );
    assert_eq!(
        kinds(&step),
        ["model_responded", "tool_requested", "tool_requested"]
    );
    assert_eq!(effects(&step), ["check_policy", "check_policy"]);
    assert_eq!(state.phase, Phase::AwaitPolicy);
}

#[test]
fn model_failure_ends_the_run_with_error() {
    let mut state = awaiting_model();
    let step = drive(&mut state, Input::ModelFailed(ProviderError::Overloaded));
    assert_eq!(kinds(&step), ["run_finished"]);
    assert!(matches!(state.finish, Some(FinishReason::Error { .. })));
}

#[test]
fn ask_emits_approval_needed_and_waits() {
    let a = call(1, "write");
    let mut state = awaiting_policy(std::slice::from_ref(&a));
    let step = drive(&mut state, decide(&a, Decision::Ask));
    assert_eq!(kinds(&step), ["approval_needed"]);
    assert!(step.effects.is_empty());
    assert_eq!(state.phase, Phase::AwaitPolicy);
}

#[test]
fn last_decision_starts_the_first_allowed_tool() {
    let (a, b) = (call(1, "read"), call(2, "write"));
    let mut state = awaiting_policy(&[a.clone(), b.clone()]);
    let first = drive(&mut state, decide(&a, Decision::Allow));
    assert_eq!(kinds(&first), ["approval_resolved"]);
    assert!(first.effects.is_empty(), "b ist noch nicht entschieden");

    let second = drive(&mut state, decide(&b, Decision::Allow));
    assert_eq!(kinds(&second), ["approval_resolved", "tool_started"]);
    assert_eq!(second.effects, vec![Effect::RunTool(a)]);
    assert_eq!(state.phase, Phase::RunTools);
}

#[test]
fn denial_goes_back_to_the_model_as_error_result() {
    let a = call(1, "write");
    let mut state = awaiting_policy(std::slice::from_ref(&a));
    let step = drive(&mut state, decide(&a, Decision::Deny));
    assert_eq!(
        kinds(&step),
        ["approval_resolved", "tool_finished", "model_requested"]
    );
    assert_eq!(effects(&step), ["call_model"]);
    assert!(matches!(
        step.events[1],
        Event::ToolFinished { is_error: true, .. }
    ));
    assert_eq!(state.phase, Phase::AwaitModel);
}

#[test]
fn rejected_call_skips_policy_and_reports_to_model() {
    let a = call(1, "gibt_es_nicht");
    let mut state = awaiting_policy(std::slice::from_ref(&a));
    let step = drive(
        &mut state,
        Input::ToolRejected {
            call_id: a.id,
            message: "Unbekanntes Tool".into(),
        },
    );
    assert_eq!(kinds(&step), ["tool_finished", "model_requested"]);
    let Effect::CallModel(request) = &step.effects[0] else {
        panic!("kein CallModel")
    };
    assert_eq!(request.messages.last().map(|m| m.role), Some(Role::Tool));
}

#[test]
fn tools_run_one_after_another_then_model_is_asked_again() {
    let (a, b) = (call(1, "read"), call(2, "write"));
    let mut state = awaiting_policy(&[a.clone(), b.clone()]);
    drive(&mut state, decide(&a, Decision::Allow));
    drive(&mut state, decide(&b, Decision::Allow));

    let more_open = drive(&mut state, done(&a));
    assert_eq!(kinds(&more_open), ["tool_finished", "tool_started"]);
    assert_eq!(more_open.effects, vec![Effect::RunTool(b.clone())]);
    assert_eq!(state.phase, Phase::RunTools);

    let last = drive(&mut state, done(&b));
    assert_eq!(kinds(&last), ["tool_finished", "model_requested"]);
    assert_eq!(effects(&last), ["call_model"]);
    assert_eq!((state.phase, state.step), (Phase::AwaitModel, 2));
}

#[test]
fn fatal_tool_error_ends_the_run() {
    let a = call(1, "read");
    let mut state = awaiting_policy(std::slice::from_ref(&a));
    drive(&mut state, decide(&a, Decision::Allow));
    let step = drive(
        &mut state,
        Input::ToolFatal {
            call_id: a.id,
            message: "Platte weg".into(),
        },
    );
    assert_eq!(kinds(&step), ["tool_finished", "run_finished"]);
    assert_eq!(
        state.finish,
        Some(FinishReason::Error {
            message: "Platte weg".into()
        })
    );
}

#[test]
fn cancel_and_deadline_end_any_running_phase() {
    let a = call(1, "read");
    let mut in_tools = awaiting_policy(std::slice::from_ref(&a));
    drive(&mut in_tools, decide(&a, Decision::Allow));
    for mut state in [
        awaiting_model(),
        awaiting_policy(std::slice::from_ref(&a)),
        in_tools,
    ] {
        state.started_at = Some(Timestamp(1_000));
        assert!(
            step(&state, Input::Tick(Timestamp(60_999)))
                .events
                .is_empty(),
            "Frist noch nicht erreicht"
        );
        let late = step(&state, Input::Tick(Timestamp(61_000)));
        assert_eq!(
            late.events,
            vec![Event::RunFinished {
                reason: FinishReason::LimitReached {
                    limit: LimitKind::Time
                }
            }]
        );
        let cancelled = drive(&mut state, Input::Cancel);
        assert_eq!(
            cancelled.events,
            vec![Event::RunFinished {
                reason: FinishReason::Cancelled
            }]
        );
        assert!(cancelled.effects.is_empty());
        assert_eq!(state.phase, Phase::Done);
    }
}

#[test]
fn limits_end_the_run() {
    // Token-Budget
    let mut state = awaiting_model();
    let step = drive(&mut state, model_done(&[call(1, "read")], 1_001));
    assert_eq!(kinds(&step), ["model_responded", "run_finished"]);
    assert_eq!(
        state.finish,
        Some(FinishReason::LimitReached {
            limit: LimitKind::Tokens
        })
    );

    // Tool-Aufrufe pro Schritt
    let mut state = awaiting_model();
    let many: Vec<ToolCall> = (1..=4).map(|n| call(n, "read")).collect();
    drive(&mut state, model_done(&many, 1));
    assert_eq!(
        state.finish,
        Some(FinishReason::LimitReached {
            limit: LimitKind::ToolCalls
        })
    );

    // Schritte: nach drei Modellanfragen ist Schluss.
    let mut state = awaiting_model();
    for n in 1..=3u128 {
        let a = call(n, "read");
        drive(&mut state, model_done(std::slice::from_ref(&a), 1));
        drive(&mut state, decide(&a, Decision::Allow));
        let last = drive(&mut state, done(&a));
        if n < 3 {
            assert_eq!(kinds(&last), ["tool_finished", "model_requested"]);
        } else {
            assert_eq!(kinds(&last), ["tool_finished", "run_finished"]);
        }
    }
    assert_eq!(
        state.finish,
        Some(FinishReason::LimitReached {
            limit: LimitKind::Steps
        })
    );
}
