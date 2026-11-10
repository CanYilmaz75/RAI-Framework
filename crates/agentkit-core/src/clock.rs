//! Uhr als Trait, damit Wartezeiten in Tests ohne echte Sleeps laufen.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::event::Timestamp;

/// Quelle für Zeitpunkte und Wartezeiten.
#[async_trait]
pub trait Clock: Send + Sync {
    /// Aktueller Zeitpunkt.
    fn now(&self) -> Timestamp;

    /// Wartet, bis `duration` vergangen ist.
    async fn sleep(&self, duration: Duration);
}

/// Steuerbare Uhr für Tests: die Zeit rückt nur durch `advance` vor.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: Mutex<u64>,
    changed: Notify,
}

impl ManualClock {
    /// Uhr, die beim angegebenen Zeitpunkt steht.
    pub fn new(start: Timestamp) -> Self {
        Self {
            now: Mutex::new(start.0),
            changed: Notify::new(),
        }
    }

    /// Rückt die Zeit vor und weckt alle Wartenden, deren Frist erreicht ist.
    pub fn advance(&self, duration: Duration) {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        if let Ok(mut now) = self.now.lock() {
            *now = now.saturating_add(millis);
        }
        self.changed.notify_waiters();
    }

    fn read(&self) -> u64 {
        self.now.lock().map(|now| *now).unwrap_or(u64::MAX)
    }
}

#[async_trait]
impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.read())
    }

    async fn sleep(&self, duration: Duration) {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let target = self.read().saturating_add(millis);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Erst anmelden, dann prüfen: so geht kein `advance` dazwischen verloren.
            changed.as_mut().enable();
            if self.read() >= target {
                return;
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn advance_moves_now() {
        let clock = ManualClock::new(Timestamp(100));
        clock.advance(Duration::from_millis(50));
        assert_eq!(clock.now(), Timestamp(150));
    }

    #[tokio::test]
    async fn zero_sleep_returns_immediately() {
        ManualClock::default().sleep(Duration::ZERO).await;
    }

    #[tokio::test]
    async fn sleep_wakes_only_after_enough_time() {
        let clock = Arc::new(ManualClock::default());
        let sleeper = {
            let clock = clock.clone();
            tokio::spawn(async move { clock.sleep(Duration::from_secs(10)).await })
        };
        tokio::task::yield_now().await;
        clock.advance(Duration::from_secs(4));
        tokio::task::yield_now().await;
        assert!(!sleeper.is_finished());
        clock.advance(Duration::from_secs(6));
        sleeper.await.unwrap();
    }
}
