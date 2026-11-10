//! Schwärzung: bekannte Secret-Werte werden aus Events entfernt, bevor sie gespeichert,
//! verteilt oder dem Modell gezeigt werden.

use can_core::{Event, SecretsHandle};
use serde::{Serialize, de::DeserializeOwned};

/// Ersatztext für einen geschwärzten Wert.
pub const REDACTED: &str = "[geschwärzt]";
/// Kürzere Werte werden nicht geschwärzt: sie träfen zu viel harmlosen Text.
const MIN_SECRET_LEN: usize = 6;

/// Kennt die Secret-Werte eines Agenten in der Form, in der sie in JSON stehen.
#[derive(Clone, Default)]
pub struct Redactor {
    needles: Vec<String>,
}

impl Redactor {
    /// Schwärzt alle Werte des Handles, die lang genug sind.
    pub fn new(secrets: &SecretsHandle) -> Self {
        let mut needles: Vec<String> = secrets
            .values()
            .into_iter()
            .filter(|value| value.len() >= MIN_SECRET_LEN)
            // In JSON stehen Anführungszeichen und Rückstriche maskiert; so wird auch gesucht.
            .filter_map(|value| serde_json::to_string(value).ok())
            .map(|quoted| quoted[1..quoted.len() - 1].to_owned())
            .collect();
        // Längere zuerst, damit ein Wert, der einen anderen enthält, ganz verschwindet.
        needles.sort_by_key(|needle| std::cmp::Reverse(needle.len()));
        Self { needles }
    }

    /// Gibt es überhaupt etwas zu schwärzen?
    pub fn is_empty(&self) -> bool {
        self.needles.is_empty()
    }

    /// Ersetzt jeden bekannten Secret-Wert im Text.
    pub fn text(&self, text: &str) -> String {
        self.needles.iter().fold(text.to_owned(), |text, needle| {
            text.replace(needle, REDACTED)
        })
    }

    /// Schwärzt einen beliebigen Wert über seine JSON-Form. Enthält er kein Secret, bleibt er
    /// unverändert.
    pub fn value<T: Serialize + DeserializeOwned>(&self, value: T) -> T {
        if self.is_empty() {
            return value;
        }
        let Ok(json) = serde_json::to_string(&value) else {
            return value;
        };
        if !self.needles.iter().any(|needle| json.contains(needle)) {
            return value;
        }
        serde_json::from_str(&self.text(&json)).unwrap_or(value)
    }

    /// Schwärzt ein Event.
    pub fn event(&self, event: Event) -> Event {
        self.value(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::{CallId, Secret, ToolOutput};

    fn redactor(values: &[&str]) -> Redactor {
        let pairs = values
            .iter()
            .enumerate()
            .map(|(i, value)| (format!("k{i}"), Secret::new((*value).to_owned())));
        Redactor::new(&SecretsHandle::from_pairs(pairs))
    }

    #[test]
    fn secrets_disappear_from_events() {
        let redactor = redactor(&["sk-geheim-123", "pass\"wort\\x"]);
        let call_id = CallId::from_parts(1, 1);
        let output = ToolOutput::text("Schlüssel sk-geheim-123 und pass\"wort\\x gefunden");
        let event = redactor.event(Event::ToolFinished {
            call_id,
            output,
            is_error: false,
        });
        let Event::ToolFinished { output, .. } = event else {
            panic!()
        };
        assert_eq!(
            output.as_text(),
            "Schlüssel [geschwärzt] und [geschwärzt] gefunden"
        );
    }

    #[test]
    fn short_values_and_clean_events_are_left_alone() {
        let redactor = redactor(&["abc"]);
        assert!(redactor.is_empty());
        let event = Event::ModelRequested { step: 1 };
        assert_eq!(redactor.event(event.clone()), event);
        assert_eq!(Redactor::default().text("abc"), "abc");
    }

    #[test]
    fn longer_secret_wins_over_its_prefix() {
        let redactor = redactor(&["geheim", "geheim-lang"]);
        assert_eq!(redactor.text("x geheim-lang y"), "x [geschwärzt] y");
    }
}
