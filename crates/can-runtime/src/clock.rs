//! Echte Uhr auf Basis von `tokio`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use can_core::{Clock, Timestamp};

/// Systemzeit und echte Wartezeiten.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

#[async_trait::async_trait]
impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Timestamp(u64::try_from(millis).unwrap_or(u64::MAX))
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn now_is_after_2020_and_sleep_returns() {
        assert!(SystemClock.now() > Timestamp(1_577_836_800_000));
        SystemClock.sleep(Duration::from_secs(3)).await;
    }
}
