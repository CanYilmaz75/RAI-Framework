//! Tabellen-Tests für Randfälle: unpassende Eingaben, Neustart und Wiederaufnahme.

use super::tests::*;
use super::*;

#[test]
fn done_accepts_a_new_start_and_nothing_else() {
    let mut state = awaiting_model();
    drive(&mut state, model_done(&[], 1));
    for input in [
        Input::Cancel,
        Input::Resume,
        Input::Tick(Timestamp(u64::MAX)),
        model_done(&[], 1),
    ] {
        assert_eq!(step(&state, input), Step::default());
    }
    let again = drive(&mut state, Input::Start(Message::user("weiter")));
    assert_eq!(kinds(&again), ["run_started", "model_requested"]);
    assert_eq!(state.step, 1);
}

#[test]
fn inputs_that_do_not_fit_the_phase_are_ignored() {
    let a = call(1, "read");
    let idle = RunState::new(config());
    assert_eq!(step(&idle, Input::Cancel), Step::default());
    assert_eq!(step(&idle, Input::Resume), Step::default());
    let model = awaiting_model();
    assert_eq!(step(&model, done(&a)), Step::default());
    assert_eq!(
        step(&model, Input::Start(Message::user("nochmal"))),
        Step::default()
    );
    let policy = awaiting_policy(std::slice::from_ref(&a));
    assert_eq!(
        step(&policy, decide(&call(9, "read"), Decision::Allow)),
        Step::default()
    );
    assert_eq!(step(&policy, done(&a)), Step::default(), "nicht gestartet");
    assert_eq!(step(&policy, model_done(&[], 1)), Step::default());
}

#[test]
fn resume_repeats_what_the_log_left_open() {
    let (a, b) = (call(1, "read"), call(2, "write"));

    let model = awaiting_model();
    assert_eq!(effects(&step(&model, Input::Resume)), ["call_model"]);

    let mut policy = awaiting_policy(&[a.clone(), b.clone()]);
    drive(&mut policy, decide(&a, Decision::Allow));
    let step_policy = step(&policy, Input::Resume);
    assert_eq!(
        step_policy.effects,
        vec![Effect::CheckPolicy(b.clone())],
        "nur der offene Aufruf"
    );
    assert!(step_policy.events.is_empty());

    // Idempotentes Tool lief gerade: erneut ausführen.
    let mut tools = awaiting_policy(&[a.clone(), b.clone()]);
    drive(&mut tools, decide(&a, Decision::Allow));
    drive(&mut tools, decide(&b, Decision::Allow));
    let rerun = step(&tools, Input::Resume);
    assert!(rerun.events.is_empty());
    assert_eq!(rerun.effects, vec![Effect::RunTool(a.clone())]);

    // Nicht idempotentes Tool lief gerade: Ergebnis „unterbrochen“, nie doppelt ausführen.
    drive(&mut tools, done(&a));
    let interrupted = drive(&mut tools, Input::Resume);
    assert_eq!(kinds(&interrupted), ["tool_finished", "model_requested"]);
    assert_eq!(effects(&interrupted), ["call_model"]);
    let Event::ToolFinished {
        call_id,
        output,
        is_error,
    } = &interrupted.events[0]
    else {
        panic!()
    };
    assert_eq!((*call_id, *is_error), (b.id, true));
    assert_eq!(output.as_text(), "Unterbrochen, Ausgang unbekannt.");
}
