//! Tabellen-Tests für Verdichtung und parallele Tools.

use super::tests::*;
use super::*;
use can_core::{Event, RunConfig, ToolSpec};
use serde_json::json;

fn kinds(step: &Step) -> Vec<&'static str> {
    step.events.iter().map(Event::kind).collect()
}

fn drive(state: &mut RunState, input: Input) -> Step {
    let step = step(state, input);
    for event in &step.events {
        state.apply(event);
    }
    step
}

fn allow(c: &ToolCall) -> Input {
    Input::PolicyDecided {
        call_id: c.id,
        decision: Decision::Allow,
        by: Approver::Rule,
    }
}

fn done(c: &ToolCall, bytes: usize) -> Input {
    Input::ToolDone {
        call_id: c.id,
        output: ToolOutput::text("x".repeat(bytes)),
        is_error: false,
    }
}

/// Zustand nach `rounds` Runden mit je einem lesenden Tool, dessen Ausgabe `bytes` groß ist.
fn after_rounds(rounds: u128, bytes: usize, window: u32) -> (RunState, Step) {
    let mut config: RunConfig = config();
    config.tools =
        vec![ToolSpec::new("read", "liest", json!({})).with_effect(can_core::Effect::ReadOnly)];
    config.limits.max_steps = 100;
    config.limits.max_tokens = u64::MAX;
    config.context_window = window;
    let mut state = RunState::new(config);
    let mut last = drive(&mut state, Input::Start(Message::user("Aufgabe")));
    for round in 1..=rounds {
        let c = call(round, "read");
        drive(&mut state, model_done(std::slice::from_ref(&c), 0));
        drive(&mut state, allow(&c));
        last = drive(&mut state, done(&c, bytes));
    }
    (state, last)
}

#[test]
fn full_context_compacts_before_the_model_is_called() {
    let (mut state, last) = after_rounds(8, 3_000, 2_000);
    assert_eq!(kinds(&last), ["tool_finished", "model_requested"]);
    let [Effect::Compact { up_to_seq, .. }] = last.effects.as_slice() else {
        panic!("erwartet war eine Verdichtung: {:?}", last.effects)
    };
    assert_eq!(state.phase, Phase::AwaitModel);

    let summary = Message::assistant("vier Runden erledigt");
    let step = drive(
        &mut state,
        Input::Compacted {
            up_to_seq: *up_to_seq,
            summary,
        },
    );
    assert_eq!(kinds(&step), ["compacted"]);
    let [Effect::CallModel(request)] = step.effects.as_slice() else {
        panic!("kein CallModel")
    };
    assert_eq!(
        request.messages.len(),
        2 + 8,
        "Aufgabe, Zusammenfassung, vier Runden"
    );
    assert_eq!(state.step, 9, "die Verdichtung zählt nicht als Schritt");
}

#[test]
fn small_context_never_compacts() {
    let (_, last) = after_rounds(8, 3_000, 0);
    assert!(matches!(last.effects.as_slice(), [Effect::CallModel(_)]));
    let (_, last) = after_rounds(8, 10, 2_000);
    assert!(matches!(last.effects.as_slice(), [Effect::CallModel(_)]));
}

#[test]
fn context_too_long_compacts_once_per_possible_cut_then_fails() {
    let (mut state, _) = after_rounds(6, 10, 0);
    let too_long = || Input::ModelFailed(ProviderError::ContextTooLong { limit: 8 });

    let first = drive(&mut state, too_long());
    assert!(first.events.is_empty());
    let [Effect::Compact { up_to_seq, .. }] = first.effects.as_slice() else {
        panic!("keine Verdichtung")
    };
    drive(
        &mut state,
        Input::Compacted {
            up_to_seq: *up_to_seq,
            summary: Message::assistant("a"),
        },
    );

    // Zweiter Überlauf: jetzt bleibt nur noch die letzte Runde erhalten.
    let second = drive(&mut state, too_long());
    let [
        Effect::Compact {
            up_to_seq: further, ..
        },
    ] = second.effects.as_slice()
    else {
        panic!()
    };
    assert!(further > up_to_seq);
    drive(
        &mut state,
        Input::Compacted {
            up_to_seq: *further,
            summary: Message::assistant("b"),
        },
    );

    let third = drive(&mut state, too_long());
    assert_eq!(kinds(&third), ["run_finished"]);
    assert!(
        matches!(&state.finish, Some(FinishReason::Error { message }) if message.contains("Kontext zu lang"))
    );
}

#[test]
fn failed_or_stale_compaction() {
    let (state, last) = after_rounds(8, 3_000, 2_000);
    let [Effect::Compact { up_to_seq, .. }] = last.effects.as_slice() else {
        panic!()
    };
    let failed = step(&state, Input::CompactFailed("Netz weg".into()));
    assert!(
        matches!(&failed.events[0], Event::RunFinished { reason: FinishReason::Error { message } }
        if message == "Verdichtung fehlgeschlagen: Netz weg")
    );
    let future = Input::Compacted {
        up_to_seq: state.next_seq,
        summary: Message::assistant("x"),
    };
    assert_eq!(
        step(&state, future),
        Step::default(),
        "ersetzt nichts Künftiges"
    );

    let mut compacted = state.clone();
    drive(
        &mut compacted,
        Input::Compacted {
            up_to_seq: *up_to_seq,
            summary: Message::assistant("x"),
        },
    );
    let again = Input::Compacted {
        up_to_seq: *up_to_seq,
        summary: Message::assistant("x"),
    };
    assert_eq!(
        step(&compacted, again),
        Step::default(),
        "geht nicht über die letzte hinaus"
    );
    assert!(matches!(
        step(&state, Input::Resume).effects.as_slice(),
        [Effect::Compact { .. }]
    ));
}

#[test]
fn consecutive_read_only_tools_start_together_writes_alone() {
    // read, read, write, read: erst zwei lesende gemeinsam, dann das schreibende, dann das letzte.
    let calls = [
        call(1, "read"),
        call(2, "read"),
        call(3, "write"),
        call(4, "read"),
    ];
    let mut config = config();
    config.tools[0] = config.tools[0]
        .clone()
        .with_effect(can_core::Effect::ReadOnly);
    config.limits.max_tool_calls_per_step = 8;
    let mut state = RunState::new(config);
    drive(&mut state, Input::Start(Message::user("los")));
    drive(&mut state, model_done(&calls, 0));
    for c in &calls[..3] {
        assert!(drive(&mut state, allow(c)).effects.is_empty());
    }
    let batch = drive(&mut state, allow(&calls[3]));
    assert_eq!(
        kinds(&batch),
        ["approval_resolved", "tool_started", "tool_started"]
    );
    assert_eq!(
        batch.effects,
        vec![
            Effect::RunTool(calls[0].clone()),
            Effect::RunTool(calls[1].clone())
        ]
    );

    // Das zweite wird zuerst fertig; das schreibende wartet, bis beide fertig sind.
    assert_eq!(
        kinds(&drive(&mut state, done(&calls[1], 1))),
        ["tool_finished"]
    );
    let write = drive(&mut state, done(&calls[0], 1));
    assert_eq!(write.effects, vec![Effect::RunTool(calls[2].clone())]);
    let read = drive(&mut state, done(&calls[2], 1));
    assert_eq!(read.effects, vec![Effect::RunTool(calls[3].clone())]);
    let model = drive(&mut state, done(&calls[3], 1));
    assert_eq!(kinds(&model), ["tool_finished", "model_requested"]);
}

#[test]
fn resume_handles_every_started_tool() {
    // `read` ist idempotent, `other` nicht: beide lesend, beide liefen beim Absturz.
    let calls = [call(1, "read"), call(2, "other")];
    let mut config = config();
    config.tools[0] = config.tools[0]
        .clone()
        .with_effect(can_core::Effect::ReadOnly);
    config
        .tools
        .push(ToolSpec::new("other", "liest", json!({})).with_effect(can_core::Effect::ReadOnly));
    let mut state = RunState::new(config);
    drive(&mut state, Input::Start(Message::user("los")));
    drive(&mut state, model_done(&calls, 0));
    drive(&mut state, allow(&calls[0]));
    drive(&mut state, allow(&calls[1]));
    let resumed = drive(&mut state, Input::Resume);
    assert_eq!(kinds(&resumed), ["tool_finished"]);
    assert!(
        matches!(&resumed.events[0], Event::ToolFinished { call_id, is_error: true, .. } if *call_id == calls[1].id)
    );
    assert_eq!(resumed.effects, vec![Effect::RunTool(calls[0].clone())]);
}

#[test]
fn a_message_during_the_model_call_waits_and_keeps_the_run_going() {
    let mut state = RunState::new(config());
    drive(&mut state, Input::Start(Message::user("erste Frage")));
    // Die Modellanfrage läuft; jetzt kommt eine Nachricht dazu.
    let injected = drive(&mut state, Input::Inject(Message::user("und noch etwas")));
    assert_eq!(kinds(&injected), ["message_injected"]);
    assert!(injected.effects.is_empty());
    assert_eq!(
        state.injected.len(),
        1,
        "sie wartet hinter der laufenden Antwort"
    );
    assert_eq!(state.messages.len(), 1);

    // Die Antwort ohne Tool-Aufruf beendet den Lauf nicht: die neue Nachricht verlangt eine Antwort.
    let answered = drive(&mut state, model_done(&[], 1));
    assert_eq!(kinds(&answered), ["model_responded", "model_requested"]);
    let [Effect::CallModel(request)] = answered.effects.as_slice() else {
        panic!("kein CallModel")
    };
    let texts: Vec<String> = request.messages.iter().map(Message::text).collect();
    assert_eq!(texts, ["erste Frage", "text", "und noch etwas"]);
    assert!(state.injected.is_empty());

    // Außerhalb eines Laufs passiert nichts.
    drive(&mut state, model_done(&[], 1));
    assert_eq!(state.phase, Phase::Done);
    assert_eq!(
        step(&state, Input::Inject(Message::user("x"))),
        Step::default()
    );
}
