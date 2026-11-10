//! Baut aus dem Zustand die Anfrage, die das Modell sieht. Das Event-Log wird nie gekürzt.

use can_core::{ContentBlock, Message, OutputBlock, Request, Role, RunState, ToolDecl, ToolOutput};

/// So viele letzte Runden (Modellantwort samt Tool-Ergebnissen) bleiben immer wörtlich erhalten.
pub const KEEP_ROUNDS: usize = 4;
/// Ab dieser Größe gilt eine alte Tool-Ausgabe als groß und wird durch ihre Vorschau ersetzt.
const BIG_OUTPUT_BYTES: usize = 2_000;
/// Länge der Vorschau einer gekürzten Tool-Ausgabe.
const PREVIEW_BYTES: usize = 400;

/// Schätzt die Token einer Anfrage: Bytes durch vier. Kein Tokenizer pro Anbieter.
pub fn estimate_tokens(request: &Request) -> u64 {
    let bytes = serde_json::to_string(request)
        .map(|json| json.len())
        .unwrap_or(usize::MAX);
    (bytes / 4) as u64
}

/// Schwelle, ab der verdichtet wird: 80 % des Kontextfensters abzüglich der Antwortreserve.
/// `None`, wenn das Kontextfenster unbekannt ist.
pub fn budget(state: &RunState) -> Option<u64> {
    let window = u64::from(state.config.context_window);
    let usable = window.saturating_sub(u64::from(state.config.reserve_output));
    (window > 0).then_some(usable * 8 / 10)
}

/// Index der ersten Nachricht, die nicht von der letzten Verdichtung ersetzt ist.
fn first_kept(state: &RunState) -> usize {
    match &state.compaction {
        Some(compaction) => state
            .message_seqs
            .partition_point(|seq| *seq <= compaction.up_to_seq),
        None => 0,
    }
}

/// Index, vor dem geschnitten werden darf: der Beginn der `keep_rounds`-letzten Runde. Der
/// Schnitt liegt immer vor einer Modellantwort, trennt also nie Aufruf und Ergebnis.
fn cut_index(state: &RunState, keep_rounds: usize) -> Option<usize> {
    let start = first_kept(state);
    let rounds: Vec<usize> = (start..state.messages.len())
        .filter(|index| state.messages[*index].role == Role::Assistant)
        .collect();
    // Mindestens eine ganze Runde muss wegfallen, sonst lohnt die Zusammenfassung nicht.
    let keep = keep_rounds.max(1);
    (rounds.len() > keep).then(|| rounds[rounds.len() - keep])
}

fn preview(output: &ToolOutput) -> ToolOutput {
    let text = output.as_text();
    if text.len() <= BIG_OUTPUT_BYTES {
        return output.clone();
    }
    let mut end = PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let note = format!(
        "{}\n… [alte Ausgabe gekürzt, {} Bytes insgesamt]",
        &text[..end],
        text.len()
    );
    ToolOutput(vec![OutputBlock::Text(note)])
}

fn with_previews(message: &Message) -> Message {
    let content = message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                call_id,
                output,
                is_error,
            } => ContentBlock::ToolResult {
                call_id: *call_id,
                output: preview(output),
                is_error: *is_error,
            },
            other => other.clone(),
        })
        .collect();
    Message {
        role: message.role,
        content,
    }
}

/// Was das Modell vom Verlauf sieht: nach einer Verdichtung die ursprüngliche Aufgabe, die
/// Zusammenfassung und alles danach.
fn visible(state: &RunState, shorten_old: bool) -> Vec<Message> {
    let start = first_kept(state);
    let mut messages = Vec::new();
    if let Some(compaction) = &state.compaction {
        let task = state.task_seq.filter(|seq| *seq <= compaction.up_to_seq);
        let task = task.and_then(|seq| state.message_seqs.iter().position(|other| *other == seq));
        messages.extend(task.map(|index| state.messages[index].clone()));
        let summary = format!(
            "Zusammenfassung des bisherigen Verlaufs:\n{}",
            compaction.summary.text()
        );
        messages.push(Message::user(summary));
    }
    let old_until = if shorten_old {
        cut_index(state, KEEP_ROUNDS).unwrap_or(start)
    } else {
        start
    };
    for (index, message) in state.messages.iter().enumerate().skip(start) {
        messages.push(if index < old_until {
            with_previews(message)
        } else {
            message.clone()
        });
    }
    messages
}

fn request(state: &RunState, shorten_old: bool) -> Request {
    let mut tools: Vec<ToolDecl> = state.config.tools.iter().map(|spec| spec.decl()).collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Request {
        model: state.config.model.clone(),
        system: state.config.system.clone(),
        messages: visible(state, shorten_old),
        tools,
        max_output: state.config.max_output,
        extra: state.config.extra.clone(),
    }
}

fn over_budget(state: &RunState, request: &Request) -> bool {
    budget(state).is_some_and(|budget| estimate_tokens(request) > budget)
}

/// Anfrage aus Konfiguration und Verlauf. Tools stehen alphabetisch sortiert, damit der Anfang
/// der Anfrage zwischen Schritten gleich bleibt und ein Prompt-Cache ihn wiederverwenden kann.
/// Wird es eng, ersetzt die Vorstufe alte, große Tool-Ausgaben durch ihre Vorschau.
pub fn build_request(state: &RunState) -> Request {
    let full = request(state, false);
    if over_budget(state, &full) {
        request(state, true)
    } else {
        full
    }
}

/// Plant eine Verdichtung, die die letzten `keep_rounds` Runden erhält. Liefert die Position,
/// bis zu der zusammengefasst wird, und die Anfrage an das Modell; `None`, wenn nichts mehr
/// zusammenzufassen ist.
pub fn plan_compaction(state: &RunState, keep_rounds: usize) -> Option<(u64, Request)> {
    let cut = cut_index(state, keep_rounds)?;
    let up_to_seq = *state.message_seqs.get(cut - 1)?;
    let mut transcript = String::new();
    if let Some(compaction) = &state.compaction {
        transcript.push_str(&format!(
            "Frühere Zusammenfassung:\n{}\n\n",
            compaction.summary.text()
        ));
    }
    for message in &state.messages[first_kept(state)..cut] {
        render(&with_previews(message), &mut transcript);
    }
    let system = "Du verdichtest den Verlauf eines Agenten-Laufs. Fasse zusammen, was erledigt ist, \
                  welche Ergebnisse und Fakten vorliegen und was offen ist. Erfinde nichts.";
    let request = Request {
        model: state.config.model.clone(),
        system: Some(system.to_owned()),
        messages: vec![Message::user(format!(
            "Fasse diesen Verlauf zusammen:\n\n{transcript}"
        ))],
        tools: Vec::new(),
        max_output: Some(1_024),
        extra: state.config.extra.clone(),
    };
    Some((up_to_seq, request))
}

/// Ist vor der nächsten Modellanfrage eine Verdichtung fällig? Nur wenn die Anfrage auch nach
/// der Vorstufe die Schwelle überschreitet.
pub fn compaction_due(state: &RunState) -> Option<(u64, Request)> {
    over_budget(state, &build_request(state))
        .then(|| plan_compaction(state, KEEP_ROUNDS))
        .flatten()
}

fn render(message: &Message, out: &mut String) {
    let speaker = match message.role {
        Role::System => "System",
        Role::User => "Nutzer",
        Role::Assistant => "Assistent",
        Role::Tool => "Tool",
    };
    for block in &message.content {
        match block {
            ContentBlock::Text(text) => out.push_str(&format!("{speaker}: {text}\n")),
            ContentBlock::ToolUse { name, input, .. } => {
                out.push_str(&format!("{speaker} ruft {name} auf mit {input}\n"));
            }
            ContentBlock::ToolResult {
                output, is_error, ..
            } => {
                let kind = if *is_error { "Fehler" } else { "Ergebnis" };
                out.push_str(&format!("Tool-{kind}: {}\n", output.as_text()));
            }
            ContentBlock::Image { media_type, .. } => {
                out.push_str(&format!("{speaker}: [Bild {media_type}]\n"))
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
