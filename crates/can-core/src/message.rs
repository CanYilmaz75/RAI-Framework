//! Anbieterneutrales Nachrichtenformat. Jeder Provider übersetzt in beide Richtungen.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::event::ToolCall;
use crate::ids::CallId;

/// Absender einer Nachricht.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Eine Nachricht im Gesprächsverlauf.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// Ein Inhaltsblock einer Nachricht.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentBlock {
    Text(String),
    Image {
        media_type: String,
        data: Bytes,
    },
    ToolUse {
        id: CallId,
        name: String,
        input: Value,
    },
    ToolResult {
        call_id: CallId,
        output: ToolOutput,
        is_error: bool,
    },
    Reasoning {
        text: String,
        signature: Option<String>,
    },
}

impl Message {
    /// Nutzernachricht aus reinem Text.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    /// Assistentennachricht aus reinem Text.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    /// Alle Textblöcke aneinandergehängt.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text(text) = block {
                out.push_str(text);
            }
        }
        out
    }

    /// Alle Tool-Aufrufe der Nachricht in ihrer Reihenfolge.
    pub fn tool_uses(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                    id: *id,
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

impl From<&str> for Message {
    fn from(text: &str) -> Self {
        Self::user(text)
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Self::user(text)
    }
}

/// Ergebnis eines Tools: eine Liste aus Text- und Bildblöcken.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolOutput(pub Vec<OutputBlock>);

/// Ein Block einer Tool-Ausgabe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputBlock {
    Text(String),
    Image { media_type: String, data: Bytes },
}

impl ToolOutput {
    /// Ausgabe aus einem einzelnen Textblock.
    pub fn text(text: impl Into<String>) -> Self {
        Self(vec![OutputBlock::Text(text.into())])
    }

    /// Alle Textblöcke aneinandergehängt.
    pub fn as_text(&self) -> String {
        let mut out = String::new();
        for block in &self.0 {
            if let OutputBlock::Text(text) = block {
                out.push_str(text);
            }
        }
        out
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

/// Rohdaten, als Hex-Text serialisiert.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut hex = String::with_capacity(self.0.len() * 2);
        for byte in &self.0 {
            hex.push_str(&format!("{byte:02x}"));
        }
        serializer.serialize_str(&hex)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        if !hex.is_ascii() || hex.len() % 2 != 0 {
            return Err(serde::de::Error::custom("ungültige Hex-Daten"));
        }
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map(Bytes)
            .map_err(|_| serde::de::Error::custom("ungültige Hex-Daten"))
    }
}

/// Token-Verbrauch einer oder mehrerer Modellantworten.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

impl Usage {
    /// Addiert einen weiteren Verbrauch, ohne überzulaufen.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(other.cached_tokens);
    }

    /// Eingabe- plus Ausgabe-Token.
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Grund, aus dem das Modell seine Antwort beendet hat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip<T: Serialize + for<'a> Deserialize<'a> + PartialEq + std::fmt::Debug>(value: &T) {
        let json = serde_json::to_string(value).unwrap();
        assert_eq!(&serde_json::from_str::<T>(&json).unwrap(), value, "{json}");
    }

    #[test]
    fn every_content_block_roundtrips() {
        let id = CallId::from_parts(1, 1);
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text("hallo".into()),
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: Bytes(vec![0, 255, 16]),
                },
                ContentBlock::ToolUse {
                    id,
                    name: "read_file".into(),
                    input: json!({"path": "a"}),
                },
                ContentBlock::ToolResult {
                    call_id: id,
                    output: ToolOutput::text("ok"),
                    is_error: false,
                },
                ContentBlock::Reasoning {
                    text: "hm".into(),
                    signature: Some("sig".into()),
                },
            ],
        };
        roundtrip(&message);
    }

    #[test]
    fn constructors_and_text() {
        assert_eq!(Message::user("a").role, Role::User);
        assert_eq!(Message::assistant("b").text(), "b");
        assert_eq!(Message::from("c"), Message::user("c"));
        assert_eq!(Message::from(String::from("d")), Message::user("d"));
    }

    #[test]
    fn tool_uses_keeps_order() {
        let (a, b) = (CallId::from_parts(1, 1), CallId::from_parts(1, 2));
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: a,
                    name: "x".into(),
                    input: json!({}),
                },
                ContentBlock::Text("dazwischen".into()),
                ContentBlock::ToolUse {
                    id: b,
                    name: "y".into(),
                    input: json!({}),
                },
            ],
        };
        let ids: Vec<CallId> = message.tool_uses().iter().map(|call| call.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    #[test]
    fn tool_output_text_ignores_images() {
        let output = ToolOutput(vec![
            OutputBlock::Text("a".into()),
            OutputBlock::Image {
                media_type: "image/png".into(),
                data: Bytes(vec![1]),
            },
            OutputBlock::Text("b".into()),
        ]);
        assert_eq!(output.as_text(), "ab");
        assert_eq!(ToolOutput::from("x"), ToolOutput::from(String::from("x")));
        roundtrip(&output);
    }

    #[test]
    fn bytes_reject_bad_hex() {
        assert!(serde_json::from_str::<Bytes>("\"abc\"").is_err());
        assert!(serde_json::from_str::<Bytes>("\"zz\"").is_err());
        assert_eq!(
            serde_json::to_string(&Bytes(vec![10, 171])).unwrap(),
            "\"0aab\""
        );
    }

    #[test]
    fn usage_adds_and_totals() {
        let mut usage = Usage {
            input_tokens: 1,
            output_tokens: 2,
            cached_tokens: 3,
        };
        usage.add(Usage {
            input_tokens: 10,
            output_tokens: u64::MAX,
            cached_tokens: 0,
        });
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, u64::MAX);
        assert_eq!(usage.total(), u64::MAX);
        roundtrip(&usage);
        roundtrip(&StopReason::MaxTokens);
    }
}
