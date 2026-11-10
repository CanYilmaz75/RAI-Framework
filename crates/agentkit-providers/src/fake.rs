//! Fake-Provider: spielt ein vorgegebenes Skript ab und merkt sich alle Anfragen.

use std::collections::VecDeque;
use std::sync::Mutex;

use agentkit_core::{
    CallId, Capabilities, Chunk, Provider, ProviderError, ProviderStream, Request, StopReason,
    Usage,
};
use async_trait::async_trait;
use serde_json::Value;

type Turn = Result<Vec<Chunk>, ProviderError>;

/// Provider für Tests: jede Anfrage verbraucht den nächsten Zug des Skripts.
pub struct FakeProvider {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<Request>>,
    capabilities: Capabilities,
    next_call: u128,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProvider {
    /// Leeres Skript; jede Anfrage schlägt fehl, bis Züge hinzugefügt sind.
    pub fn new() -> Self {
        Self {
            turns: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            capabilities: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                images: false,
                prompt_cache: false,
                context_window: 100_000,
                max_output: 4_096,
            },
            next_call: 1,
        }
    }

    /// Skript aus fertigen Zügen, etwa aus einer Aufzeichnung.
    pub fn from_turns(turns: Vec<Vec<Chunk>>) -> Self {
        let mut provider = Self::new();
        for chunks in turns {
            provider = provider.turn(chunks);
        }
        provider
    }

    /// Fügt einen Zug aus beliebigen Chunks an.
    pub fn turn(self, chunks: Vec<Chunk>) -> Self {
        self.push(Ok(chunks))
    }

    /// Fügt eine reine Textantwort an.
    pub fn text(self, text: &str) -> Self {
        self.turn(vec![
            Chunk::TextDelta {
                text: text.to_owned(),
            },
            done(StopReason::EndTurn),
        ])
    }

    /// Fügt eine Antwort mit genau einem Tool-Aufruf an.
    pub fn tool_call(self, name: &str, input: Value) -> Self {
        self.tool_calls(vec![(name, input)])
    }

    /// Fügt eine Antwort mit mehreren Tool-Aufrufen an. Die Kennungen sind fortlaufend und fest.
    pub fn tool_calls(mut self, calls: Vec<(&str, Value)>) -> Self {
        let mut chunks = Vec::new();
        for (index, (name, input)) in (0u32..).zip(calls) {
            let id = CallId::from_parts(0, self.next_call);
            self.next_call += 1;
            chunks.push(Chunk::ToolCallStart {
                index,
                id,
                name: name.to_owned(),
            });
            chunks.push(Chunk::ToolCallArgs {
                index,
                delta: input.to_string(),
            });
        }
        chunks.push(done(StopReason::ToolUse));
        self.turn(chunks)
    }

    /// Fügt einen Zug an, der mit einem Fehler endet.
    pub fn fail(self, error: ProviderError) -> Self {
        self.push(Err(error))
    }

    /// Setzt die gemeldeten Fähigkeiten.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Alle bisher empfangenen Anfragen.
    pub fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }

    fn push(self, turn: Turn) -> Self {
        if let Ok(mut turns) = self.turns.lock() {
            turns.push_back(turn);
        }
        self
    }
}

fn done(stop: StopReason) -> Chunk {
    Chunk::Done {
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
        },
        stop,
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn capabilities(&self, _model: &str) -> Capabilities {
        self.capabilities
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(req);
        }
        let turn = self
            .turns
            .lock()
            .ok()
            .and_then(|mut turns| turns.pop_front());
        match turn {
            Some(Ok(chunks)) => Ok(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok)))),
            Some(Err(error)) => Err(error),
            None => Err(ProviderError::Protocol(
                "FakeProvider: Skript ist erschöpft".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{Assembler, Message};
    use futures::StreamExt;
    use serde_json::json;

    async fn next_message(provider: &FakeProvider) -> Result<Message, ProviderError> {
        let mut stream = provider.stream(Request::default()).await?;
        let mut assembler = Assembler::default();
        while let Some(chunk) = stream.next().await {
            assembler.push(chunk?)?;
        }
        Ok(assembler.finish()?.0)
    }

    #[tokio::test]
    async fn plays_turns_in_order_then_runs_dry() {
        let provider = FakeProvider::new()
            .tool_call("read_file", json!({"path": "a"}))
            .text("fertig");
        let first = next_message(&provider).await.unwrap();
        assert_eq!(first.tool_uses()[0].name, "read_file");
        assert_eq!(first.tool_uses()[0].input, json!({"path": "a"}));
        assert_eq!(next_message(&provider).await.unwrap().text(), "fertig");
        assert!(matches!(
            next_message(&provider).await,
            Err(ProviderError::Protocol(_))
        ));
        assert_eq!(provider.requests().len(), 3);
    }

    #[tokio::test]
    async fn call_ids_are_fixed_and_distinct() {
        let build = || FakeProvider::new().tool_calls(vec![("a", json!({})), ("b", json!({}))]);
        let first = next_message(&build()).await.unwrap().tool_uses();
        let second = next_message(&build()).await.unwrap().tool_uses();
        assert_eq!(first, second);
        assert_ne!(first[0].id, first[1].id);
    }

    #[tokio::test]
    async fn fail_and_from_turns() {
        let provider = FakeProvider::default().fail(ProviderError::Overloaded);
        assert!(matches!(
            next_message(&provider).await,
            Err(ProviderError::Overloaded)
        ));
        let turns = vec![vec![
            Chunk::TextDelta { text: "x".into() },
            Chunk::Done {
                usage: Usage::default(),
                stop: StopReason::EndTurn,
            },
        ]];
        assert_eq!(
            next_message(&FakeProvider::from_turns(turns))
                .await
                .unwrap()
                .text(),
            "x"
        );
    }

    #[test]
    fn capabilities_can_be_overridden() {
        let mut capabilities = FakeProvider::new().capabilities("m");
        assert!(capabilities.tools);
        capabilities.tools = false;
        assert!(
            !FakeProvider::new()
                .with_capabilities(capabilities)
                .capabilities("m")
                .tools
        );
    }
}
