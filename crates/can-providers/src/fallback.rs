//! `Fallback<A, B>`: fällt der erste Provider aus, übernimmt der zweite.
//!
//! Gewechselt wird nur, solange noch kein Chunk weitergegeben wurde, und nur bei Fehlern, die
//! am Anbieter liegen: Überlast, Rate-Limit, Netzwerk und Anmeldung.

use async_trait::async_trait;
use can_core::{Capabilities, Provider, ProviderError, ProviderStream, Request};
use futures::StreamExt;

/// Zwei Provider hintereinander. `secondary_model` ersetzt den Modellnamen der Anfrage, wenn der
/// zweite Anbieter das Modell anders nennt.
pub struct Fallback<A, B> {
    primary: A,
    secondary: B,
    secondary_model: Option<String>,
}

impl<A: Provider, B: Provider> Fallback<A, B> {
    /// `secondary` springt ein, wenn `primary` ausfällt.
    pub fn new(primary: A, secondary: B) -> Self {
        Self {
            primary,
            secondary,
            secondary_model: None,
        }
    }

    /// Modellname beim zweiten Anbieter.
    pub fn with_secondary_model(mut self, model: impl Into<String>) -> Self {
        self.secondary_model = Some(model.into());
        self
    }
}

fn should_switch(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Overloaded
            | ProviderError::RateLimited { .. }
            | ProviderError::Network(_)
            | ProviderError::Auth
    )
}

#[async_trait]
impl<A: Provider, B: Provider> Provider for Fallback<A, B> {
    /// Die vorsichtigere Angabe beider Anbieter, damit eine Anfrage bei beiden passt.
    fn capabilities(&self, model: &str) -> Capabilities {
        let first = self.primary.capabilities(model);
        let second = self
            .secondary
            .capabilities(self.secondary_model.as_deref().unwrap_or(model));
        Capabilities {
            tools: first.tools && second.tools,
            parallel_tool_calls: first.parallel_tool_calls && second.parallel_tool_calls,
            images: first.images && second.images,
            prompt_cache: first.prompt_cache && second.prompt_cache,
            context_window: first.context_window.min(second.context_window),
            max_output: first.max_output.min(second.max_output),
        }
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let error = match self.primary.stream(req.clone()).await {
            Ok(mut stream) => match stream.next().await {
                Some(Err(error)) => error,
                Some(Ok(first)) => {
                    return Ok(Box::pin(futures::stream::iter([Ok(first)]).chain(stream)));
                }
                None => return Ok(Box::pin(futures::stream::empty())),
            },
            Err(error) => error,
        };
        if !should_switch(&error) {
            return Err(error);
        }
        let mut req = req;
        if let Some(model) = &self.secondary_model {
            req.model = model.clone();
        }
        self.secondary.stream(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeProvider;
    use can_core::Chunk;

    async fn text(provider: &impl Provider, model: &str) -> Result<String, ProviderError> {
        let mut stream = provider
            .stream(Request {
                model: model.into(),
                ..Request::default()
            })
            .await?;
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            if let Chunk::TextDelta { text } = item? {
                out.push_str(&text);
            }
        }
        Ok(out)
    }

    #[tokio::test]
    async fn secondary_takes_over_when_the_primary_is_down() {
        let fallback = Fallback::new(
            FakeProvider::new()
                .fail(ProviderError::Overloaded)
                .text("erster"),
            FakeProvider::new().text("zweiter"),
        )
        .with_secondary_model("ersatz-modell");
        assert_eq!(text(&fallback, "haupt-modell").await.unwrap(), "zweiter");
        assert_eq!(fallback.secondary.requests()[0].model, "ersatz-modell");
        // Läuft der erste wieder, bleibt der zweite unberührt.
        assert_eq!(text(&fallback, "haupt-modell").await.unwrap(), "erster");
        assert_eq!(fallback.secondary.requests().len(), 1);
    }

    #[tokio::test]
    async fn request_errors_do_not_switch() {
        let fallback = Fallback::new(
            FakeProvider::new()
                .fail(ProviderError::ContextTooLong { limit: 8 })
                .fail(ProviderError::InvalidRequest("x".into())),
            FakeProvider::new().text("nie"),
        );
        assert!(matches!(
            text(&fallback, "m").await,
            Err(ProviderError::ContextTooLong { .. })
        ));
        assert!(matches!(
            text(&fallback, "m").await,
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(fallback.secondary.requests().is_empty());
    }

    #[test]
    fn capabilities_are_the_cautious_combination() {
        let mut small = FakeProvider::new().capabilities("m");
        small.context_window = 8_000;
        small.images = true;
        let fallback = Fallback::new(
            FakeProvider::new(),
            FakeProvider::new().with_capabilities(small),
        );
        let combined = fallback.capabilities("m");
        assert_eq!(combined.context_window, 8_000);
        assert!(!combined.images && combined.tools);
    }
}
