//! Führt ein Tool mit Zeitlimit, Abbruch und Panic-Schutz aus und kürzt seine Ausgabe.

use std::sync::Arc;
use std::time::Duration;

use can_core::{OutputBlock, Tool, ToolCall, ToolCtx, ToolError, ToolOutput};
use tokio_util::sync::CancellationToken;

/// So lange darf ein Tool nach dem Abbruchsignal aufräumen, bevor seine Task beendet wird.
const CLEANUP_GRACE: Duration = Duration::from_secs(5);

/// Ausgang eines Tool-Aufrufs aus Sicht des Treibers.
pub(crate) enum ToolRun {
    Done {
        output: ToolOutput,
        is_error: bool,
    },
    Fatal(String),
    /// Der Lauf wurde abgebrochen.
    Cancelled,
    /// Das Zeitlimit des Laufs ist erreicht.
    Deadline,
}

/// Führt `tool` aus. `run_cancel` ist das Token des Laufs, `run_remaining` seine Restzeit.
pub(crate) async fn run_tool(
    tool: Arc<dyn Tool>,
    call: &ToolCall,
    cx: ToolCtx,
    run_cancel: &CancellationToken,
    run_remaining: Duration,
) -> ToolRun {
    let spec = tool.spec().clone();
    let clock = cx.clock.clone();
    let tool_cancel = cx.cancel.clone();
    let args = call.input.clone();
    // Eigene Task: ein Panic im Tool reißt den Lauf nicht mit.
    let mut task = tokio::spawn(async move { tool.call(args, &cx).await });
    let outcome = tokio::select! {
        biased;
        _ = run_cancel.cancelled() => ToolRun::Cancelled,
        joined = &mut task => match joined {
            Ok(Ok(output)) => {
                ToolRun::Done { output, is_error: false }
            }
            Ok(Err(ToolError::ForModel(message))) => {
                ToolRun::Done { output: ToolOutput::text(message), is_error: true }
            }
            Ok(Err(ToolError::Fatal(error))) => ToolRun::Fatal(error.to_string()),
            Err(_) => ToolRun::Done {
                output: ToolOutput::text("Das Tool ist mit einem internen Fehler abgestürzt."),
                is_error: true,
            },
        },
        _ = clock.sleep(spec.timeout) => ToolRun::Done {
            output: ToolOutput::text(format!(
                "Zeitlimit von {} s überschritten; das Tool wurde abgebrochen.",
                spec.timeout.as_secs()
            )),
            is_error: true,
        },
        _ = clock.sleep(run_remaining) => ToolRun::Deadline,
    };
    if !task.is_finished() {
        // Sauberes Ende: das Tool bekommt das Abbruchsignal und eine Frist zum Aufräumen.
        tool_cancel.cancel();
        tokio::select! {
            _ = &mut task => {}
            _ = clock.sleep(CLEANUP_GRACE) => task.abort(),
        }
    }
    outcome
}

/// Größe aller Textblöcke zusammen.
pub(crate) fn text_bytes(output: &ToolOutput) -> usize {
    output.as_text().len()
}

/// Kürzt Textblöcke auf zusammen höchstens `max_bytes`: Anfang und Ende bleiben, die Mitte
/// wird markiert. Bildblöcke bleiben unverändert.
pub(crate) fn truncate(output: ToolOutput, max_bytes: usize) -> ToolOutput {
    let total: usize = output
        .0
        .iter()
        .map(|block| match block {
            OutputBlock::Text(text) => text.len(),
            _ => 0,
        })
        .sum();
    if total <= max_bytes {
        return output;
    }
    let text = output.as_text();
    let half = max_bytes / 2;
    let head_end = floor_boundary(&text, half);
    let tail_start = ceil_boundary(&text, text.len() - half);
    let cut = tail_start - head_end;
    let shortened = format!(
        "{}\n… [{cut} Bytes gekürzt] …\n{}",
        &text[..head_end],
        &text[tail_start..]
    );
    let mut blocks = vec![OutputBlock::Text(shortened)];
    blocks.extend(
        output
            .0
            .into_iter()
            .filter(|block| !matches!(block, OutputBlock::Text(_))),
    );
    ToolOutput(blocks)
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::Bytes;

    #[test]
    fn short_output_is_untouched() {
        let output = ToolOutput::text("kurz");
        assert_eq!(truncate(output.clone(), 100), output);
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let text = format!("ANFANG{}ENDE", "x".repeat(1_000));
        let shortened = truncate(ToolOutput::text(text), 20).as_text();
        assert!(shortened.starts_with("ANFANGxxxx\n… ["));
        assert!(shortened.ends_with("] …\nxxxxxxENDE"));
        assert!(shortened.contains("990 Bytes gekürzt"));
    }

    #[test]
    fn cut_respects_char_boundaries_and_keeps_images() {
        let image = OutputBlock::Image {
            media_type: "image/png".into(),
            data: Bytes(vec![1]),
        };
        let output = ToolOutput(vec![
            OutputBlock::Text("äöüäöüäöüäöü".into()),
            image.clone(),
        ]);
        let shortened = truncate(output, 7);
        assert!(shortened.as_text().starts_with("ä\n…"));
        assert!(shortened.as_text().ends_with("…\nü"));
        assert_eq!(shortened.0.last(), Some(&image));
    }
}
