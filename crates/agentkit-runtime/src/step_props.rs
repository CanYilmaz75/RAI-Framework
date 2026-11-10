//! Property-Tests: beliebige Eingabefolgen verletzen keine Invariante der Zustandsmaschine.

use super::tests::{call, config, model_done};
use super::*;
use agentkit_core::{Envelope, RunId, SCHEMA_VERSION};
use proptest::prelude::*;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
enum Action {
    Start,
    Resume,
    ModelText(u64),
    ModelTools(u8, u64),
    ModelFail,
    Decide(usize, u8),
    Reject(usize),
    Done(usize, bool),
    Fatal(usize),
    Stray,
    Cancel,
    Tick(u64),
}

fn actions() -> impl Strategy<Value = Vec<Action>> {
    let action = prop_oneof![
        2 => Just(Action::Start),
        1 => Just(Action::Resume),
        3 => (0u64..400).prop_map(Action::ModelText),
        6 => (1u8..5, 0u64..400).prop_map(|(n, tokens)| Action::ModelTools(n, tokens)),
        1 => Just(Action::ModelFail),
        8 => (0usize..4, 0u8..3).prop_map(|(i, d)| Action::Decide(i, d)),
        1 => (0usize..4).prop_map(Action::Reject),
        8 => (0usize..4, any::<bool>()).prop_map(|(i, e)| Action::Done(i, e)),
        1 => (0usize..4).prop_map(Action::Fatal),
        1 => Just(Action::Stray),
        1 => Just(Action::Cancel),
        1 => (0u64..120_000).prop_map(Action::Tick),
    ];
    proptest::collection::vec(action, 0..60)
}

/// Übersetzt eine Aktion in eine Eingabe; Indizes zeigen auf die offenen Aufrufe des Zustands.
fn input(action: &Action, state: &RunState, counter: u128) -> Input {
    let pick = |index: usize| match state.pending.len() {
        0 => CallId::from_parts(9, 9),
        len => state.pending[index % len].call.id,
    };
    match action {
        Action::Start => Input::Start(Message::user("los")),
        Action::Resume => Input::Resume,
        Action::ModelText(tokens) => model_done(&[], *tokens),
        Action::ModelTools(n, tokens) => {
            let names = ["read", "write", "unbekannt"];
            let calls: Vec<ToolCall> = (0..*n)
                .map(|i| call(counter * 10 + u128::from(i), names[usize::from(i) % 3]))
                .collect();
            model_done(&calls, *tokens)
        }
        Action::ModelFail => Input::ModelFailed(ProviderError::Overloaded),
        Action::Decide(index, decision) => Input::PolicyDecided {
            call_id: pick(*index),
            decision: [Decision::Allow, Decision::Ask, Decision::Deny][usize::from(*decision)],
            by: Approver::Rule,
        },
        Action::Reject(index) => Input::ToolRejected {
            call_id: pick(*index),
            message: "nein".into(),
        },
        Action::Done(index, is_error) => Input::ToolDone {
            call_id: pick(*index),
            output: ToolOutput::text("ok"),
            is_error: *is_error,
        },
        Action::Fatal(index) => Input::ToolFatal {
            call_id: pick(*index),
            message: "fatal".into(),
        },
        Action::Stray => Input::ToolDone {
            call_id: CallId::from_parts(9, 9),
            output: ToolOutput::text("?"),
            is_error: false,
        },
        Action::Cancel => Input::Cancel,
        Action::Tick(now) => Input::Tick(Timestamp(*now)),
    }
}

/// Prüft die Invarianten aus dem Abschnitt „Agent-Loop“ über das gesamte Log.
fn check_log(log: &[Envelope], max_steps: u32) -> Result<(), TestCaseError> {
    let mut allowed = HashSet::new();
    let mut open: HashMap<CallId, u32> = HashMap::new();
    let mut finished = true;
    let mut model_requests = 0;
    for (position, envelope) in log.iter().enumerate() {
        prop_assert_eq!(
            envelope.seq,
            position as u64,
            "seq ist lückenlos und streng steigend"
        );
        let event = &envelope.event;
        if finished {
            prop_assert!(
                matches!(event, Event::RunStarted { .. }),
                "Event nach RunFinished: {:?}",
                event
            );
        }
        match event {
            Event::RunStarted { .. } => {
                finished = false;
                model_requests = 0;
                open.clear();
            }
            Event::ModelRequested { .. } => {
                model_requests += 1;
                prop_assert!(
                    model_requests <= max_steps,
                    "mehr Modellanfragen als max_steps"
                );
                for (id, count) in &open {
                    prop_assert_eq!(
                        *count,
                        1,
                        "ToolRequested {} ohne genau ein ToolFinished",
                        id
                    );
                }
                open.clear();
            }
            Event::ToolRequested { call } => {
                prop_assert!(open.insert(call.id, 0).is_none());
            }
            Event::ApprovalResolved {
                call_id,
                decision: Decision::Allow,
                ..
            } => {
                allowed.insert(*call_id);
            }
            Event::ToolStarted { call_id } => {
                prop_assert!(allowed.contains(call_id), "ToolStarted ohne Freigabe");
            }
            Event::ToolFinished { call_id, .. } => {
                let count = open.get_mut(call_id);
                prop_assert!(count.is_some(), "ToolFinished ohne ToolRequested");
                if let Some(count) = count {
                    *count += 1;
                    prop_assert_eq!(*count, 1, "ToolFinished doppelt");
                }
            }
            Event::RunFinished { .. } => finished = true,
            _ => {}
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn arbitrary_inputs_keep_all_invariants(actions in actions()) {
        let config = config();
        let run_id = RunId::from_parts(1, 1);
        let mut state = RunState::new(config.clone());
        let mut log: Vec<Envelope> = Vec::new();

        for (counter, action) in (1u128..).zip(&actions) {
            let first = step(&state, input(action, &state, counter));
            let second = step(&state, input(action, &state, counter));
            prop_assert_eq!(&first, &second, "step ist deterministisch");

            for event in first.events {
                prop_assert!(event.is_durable(), "step erzeugt nur dauerhafte Events");
                let envelope = Envelope {
                    seq: state.next_seq,
                    run_id,
                    ts: Timestamp(1_000),
                    schema: SCHEMA_VERSION,
                    event,
                };
                state.apply_envelope(&envelope);
                log.push(envelope);
            }
            if state.phase == Phase::Done {
                prop_assert!(first.effects.is_empty(), "nach dem Ende gibt es keine Effekte");
                prop_assert!(state.pending.is_empty());
            }
        }

        check_log(&log, config.limits.max_steps)?;
        prop_assert_eq!(RunState::fold(config, &log), state, "Falten des Logs ergibt den Zustand");
    }
}
