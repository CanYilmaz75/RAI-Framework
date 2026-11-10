//! `Retry<P>`: wiederholt eine Anfrage bei vorübergehenden Fehlern.
//!
//! Wiederholt wird nur bei `RateLimited`, `Overloaded` und `Network`, mit exponentiellem
//! Backoff plus Jitter, und nur solange noch kein Chunk weitergegeben wurde.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use can_core::{Capabilities, Clock, Provider, ProviderError, ProviderStream, Request};
use futures::StreamExt;

/// Dekorator um einen Provider.
pub struct Retry<P> {
    inner: P,
    clock: Arc<dyn Clock>,
    max_attempts: u32,
    base_delay: Duration,
}

impl<P: Provider> Retry<P> {
    /// Höchstens fünf Versuche, erste Wartezeit eine halbe Sekunde.
    pub fn new(inner: P, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner,
            clock,
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
        }
    }

    /// Setzt die Höchstzahl der Versuche; mindestens einer.
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Wartezeit vor dem nächsten Versuch: die Vorgabe des Anbieters oder Backoff plus Jitter.
    fn delay(&self, error: &ProviderError, attempt: u32) -> Duration {
        if let ProviderError::RateLimited {
            retry_after: Some(wait),
        } = error
        {
            return *wait;
        }
        let base = u64::try_from(self.base_delay.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let backoff = base.saturating_mul(1u64 << attempt.saturating_sub(1).min(16));
        // Jitter aus der Uhrzeit: verteilt gleichzeitige Wiederholungen, ohne Zufallsquelle.
        let jitter = self
            .clock
            .now()
            .0
            .wrapping_mul(2_654_435_761)
            .wrapping_add(u64::from(attempt))
            % base;
        Duration::from_millis(backoff.saturating_add(jitter))
    }
}

/// Lohnt sich ein weiterer Versuch?
pub fn is_retryable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::RateLimited { .. } | ProviderError::Overloaded | ProviderError::Network(_)
    )
}

#[async_trait]
impl<P: Provider> Provider for Retry<P> {
    fn capabilities(&self, model: &str) -> Capabilities {
        self.inner.capabilities(model)
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let mut attempt = 1;
        loop {
            // Der erste Chunk entscheidet: scheitert schon er, ist noch nichts weitergegeben.
            let error = match self.inner.stream(req.clone()).await {
                Ok(mut stream) => match stream.next().await {
                    Some(Err(error)) => error,
                    Some(Ok(first)) => {
                        return Ok(Box::pin(futures::stream::iter([Ok(first)]).chain(stream)));
                    }
                    None => return Ok(Box::pin(futures::stream::empty())),
                },
                Err(error) => error,
            };
            if !is_retryable(&error) || attempt >= self.max_attempts {
                return Err(error);
            }
            self.clock.sleep(self.delay(&error, attempt)).await;
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeProvider;
    use can_core::{Chunk, ManualClock, Timestamp};

    fn clock() -> Arc<ManualClock> {
        Arc::new(ManualClock::new(Timestamp(1_000)))
    }

    /// Rückt die Uhr vor, bis die Anfrage fertig ist, und liefert Ergebnis und vergangene Zeit.
    async fn run(
        retry: &Retry<FakeProvider>,
        clock: &ManualClock,
    ) -> (Result<Vec<Chunk>, ProviderError>, u64) {
        let work = async {
            let mut stream = retry.stream(Request::default()).await?;
            let mut chunks = Vec::new();
            while let Some(item) = stream.next().await {
                chunks.push(item?);
            }
            Ok(chunks)
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                result = &mut work => return (result, clock.now().0 - 1_000),
                _ = tokio::task::yield_now() => clock.advance(Duration::from_millis(50)),
            }
        }
    }

    #[tokio::test]
    async fn transient_errors_are_retried_with_growing_delay() {
        let clock = clock();
        let provider = FakeProvider::new()
            .fail(ProviderError::Overloaded)
            .fail(ProviderError::Network("Verbindung weg".into()))
            .text("endlich");
        let retry = Retry::new(provider, clock.clone());
        let (result, waited) = run(&retry, &clock).await;
        assert!(matches!(&result.unwrap()[0], Chunk::TextDelta { text } if text == "endlich"));
        // 500 ms + 1000 ms Backoff, dazu je unter 500 ms Jitter.
        assert!((1_500..2_600).contains(&waited), "{waited} ms");
        assert_eq!(retry.inner.requests().len(), 3);
    }

    #[tokio::test]
    async fn retry_after_is_respected() {
        let clock = clock();
        let limited = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
        };
        let retry = Retry::new(FakeProvider::new().fail(limited).text("ok"), clock.clone());
        let (result, waited) = run(&retry, &clock).await;
        assert!(result.is_ok());
        assert!((7_000..7_100).contains(&waited), "{waited} ms");
    }

    #[tokio::test]
    async fn permanent_errors_and_exhausted_attempts_fail() {
        let clock = clock();
        let retry = Retry::new(
            FakeProvider::new().fail(ProviderError::Auth).text("nie"),
            clock.clone(),
        );
        assert!(matches!(
            run(&retry, &clock).await.0,
            Err(ProviderError::Auth)
        ));
        assert_eq!(retry.inner.requests().len(), 1);

        let mut provider = FakeProvider::new();
        for _ in 0..6 {
            provider = provider.fail(ProviderError::Overloaded);
        }
        let retry = Retry::new(provider, clock.clone()).with_max_attempts(3);
        assert!(matches!(
            run(&retry, &clock).await.0,
            Err(ProviderError::Overloaded)
        ));
        assert_eq!(retry.inner.requests().len(), 3);
        assert!(retry.capabilities("m").tools);
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        assert!(is_retryable(&ProviderError::Overloaded));
        assert!(is_retryable(&ProviderError::RateLimited {
            retry_after: None
        }));
        assert!(!is_retryable(&ProviderError::ContextTooLong { limit: 1 }));
        assert!(!is_retryable(&ProviderError::Protocol("x".into())));
    }
}
