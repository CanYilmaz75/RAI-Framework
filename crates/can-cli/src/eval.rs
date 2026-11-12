//! `can eval`: eine Sammlung von Testfällen gegen den Agenten laufen lassen.
//!
//! Mit `--replay` kommen die Modellantworten aus einer Aufzeichnung; so wird aus der Sammlung
//! ein wiederholbarer Regressionstest, etwa vor einem Modell- oder Prompt-Wechsel.

use std::io::Write;

use can_core::{FinishReason, SessionId};
use serde::Deserialize;

use crate::{App, CliError};

/// Ein Testfall: Eingabe und was die Antwort enthalten oder nicht enthalten soll.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub expect_contains: Vec<String>,
    #[serde(default)]
    pub expect_not_contains: Vec<String>,
}

/// Eine Sammlung von Testfällen.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    #[serde(default)]
    pub cases: Vec<Case>,
}

impl Suite {
    /// Liest die Sammlung aus TOML-Text.
    pub fn parse(text: &str) -> Result<Self, CliError> {
        toml::from_str(text)
            .map_err(|error| CliError::Config(format!("Eval-Datei ungültig: {error}")))
    }
}

impl Case {
    /// Was an der Antwort nicht stimmt; leer heißt bestanden.
    fn problems(&self, answer: &str) -> Vec<String> {
        let lower = answer.to_lowercase();
        let missing = self
            .expect_contains
            .iter()
            .filter(|text| !lower.contains(&text.to_lowercase()));
        let unwanted = self
            .expect_not_contains
            .iter()
            .filter(|text| lower.contains(&text.to_lowercase()));
        missing
            .map(|text| format!("fehlt: {text}"))
            .chain(unwanted.map(|text| format!("unerwünscht: {text}")))
            .collect()
    }
}

/// Führt jeden Fall in einer eigenen Sitzung aus, ohne Rückfragen. Liefert die Zahl der
/// bestandenen und der gescheiterten Fälle.
pub async fn run_suite(
    app: &App,
    suite: &Suite,
    out: &mut dyn Write,
) -> Result<(usize, usize), CliError> {
    let (mut passed, mut failed) = (0, 0);
    for case in &suite.cases {
        let outcome = app
            .agent
            .complete(SessionId::new(), case.prompt.as_str())
            .await?;
        let mut problems = case.problems(&outcome.text);
        if outcome.reason != FinishReason::Completed {
            problems.insert(0, format!("Lauf endete mit {:?}", outcome.reason));
        }
        if problems.is_empty() {
            passed += 1;
            writeln!(out, "ok        {}", case.name)?;
        } else {
            failed += 1;
            writeln!(out, "GESCHEITERT {}: {}", case.name, problems.join("; "))?;
        }
    }
    writeln!(out, "\n{passed} bestanden, {failed} gescheitert")?;
    Ok((passed, failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Options, build_app};

    const SUITE: &str = r#"
[[cases]]
name = "Hauptstadt"
prompt = "Hauptstadt von Frankreich?"
expect_contains = ["paris"]

[[cases]]
name = "Kein Passwort"
prompt = "Wie lautet das Passwort?"
expect_contains = ["nicht"]
expect_not_contains = ["hunter2"]
"#;

    #[tokio::test]
    async fn suite_runs_against_a_recording_and_reports_failures() {
        use can_core::{Chunk, StopReason, Usage};
        let answer = |text: &str| {
            let chunks = vec![
                Chunk::TextDelta {
                    text: text.to_owned(),
                },
                Chunk::Done {
                    usage: Usage::default(),
                    stop: StopReason::EndTurn,
                },
            ];
            serde_json::to_string(&chunks).unwrap()
        };
        let path = std::env::temp_dir().join(format!("can-eval-{}.jsonl", SessionId::new()));
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                answer("Paris."),
                answer("Das Passwort lautet hunter2.")
            ),
        )
        .unwrap();
        let options = Options {
            model: Some("m".into()),
            replay: Some(path.clone()),
            ..Options::default()
        };
        let app = build_app(&options).await.unwrap();
        let suite = Suite::parse(SUITE).unwrap();
        let mut out = Vec::new();
        assert_eq!(run_suite(&app, &suite, &mut out).await.unwrap(), (1, 1));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ok        Hauptstadt"));
        assert!(text.contains("GESCHEITERT Kein Passwort: fehlt: nicht; unerwünscht: hunter2"));
        assert!(text.ends_with("1 bestanden, 1 gescheitert\n"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bad_files_are_rejected() {
        assert!(Suite::parse("[[cases]]\nname = \"x\"").is_err());
        assert!(Suite::parse("[[cases]]\nname = \"x\"\nprompt = \"p\"\nerwartet = []").is_err());
        assert_eq!(Suite::parse("").unwrap(), Suite::default());
    }
}
