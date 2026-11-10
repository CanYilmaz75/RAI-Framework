//! Kennzahlen des Dienstes im Prometheus-Textformat.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Zähler seit dem Start des Prozesses.
#[derive(Debug, Default)]
pub struct Metrics {
    runs_started: AtomicU64,
    approvals_resolved: AtomicU64,
    /// Beendete Läufe nach Grund.
    finished: Mutex<BTreeMap<String, u64>>,
    /// Verbrauchte Token nach Mandant.
    tokens: Mutex<BTreeMap<String, u64>>,
}

impl Metrics {
    pub fn run_started(&self) {
        self.runs_started.fetch_add(1, Ordering::Relaxed);
    }

    pub fn approval_resolved(&self) {
        self.approvals_resolved.fetch_add(1, Ordering::Relaxed);
    }

    /// Ein Lauf ist zu Ende: Grund und Token-Verbrauch des Mandanten.
    pub fn run_finished(&self, reason: &str, tenant: &str, tokens: u64) {
        if let Ok(mut finished) = self.finished.lock() {
            *finished.entry(reason.to_owned()).or_default() += 1;
        }
        if let Ok(mut used) = self.tokens.lock() {
            *used.entry(tenant.to_owned()).or_default() += tokens;
        }
    }

    /// Bisher verbrauchte Token eines Mandanten.
    pub fn tokens_used(&self, tenant: &str) -> u64 {
        self.tokens
            .lock()
            .ok()
            .and_then(|used| used.get(tenant).copied())
            .unwrap_or(0)
    }

    /// Alle Kennzahlen als Text; `running` und `pending` sind die aktuellen Stände.
    pub fn render(&self, running: usize, pending: usize) -> String {
        let mut out = String::new();
        out.push_str("# TYPE can_runs_started_total counter\n");
        out.push_str(&format!(
            "can_runs_started_total {}\n",
            self.runs_started.load(Ordering::Relaxed)
        ));
        out.push_str("# TYPE can_runs_finished_total counter\n");
        if let Ok(finished) = self.finished.lock() {
            for (reason, count) in finished.iter() {
                out.push_str(&format!(
                    "can_runs_finished_total{{reason=\"{reason}\"}} {count}\n"
                ));
            }
        }
        out.push_str("# TYPE can_tokens_total counter\n");
        if let Ok(tokens) = self.tokens.lock() {
            for (tenant, count) in tokens.iter() {
                out.push_str(&format!(
                    "can_tokens_total{{tenant=\"{tenant}\"}} {count}\n"
                ));
            }
        }
        out.push_str("# TYPE can_approvals_resolved_total counter\n");
        out.push_str(&format!(
            "can_approvals_resolved_total {}\n",
            self.approvals_resolved.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "# TYPE can_runs_running gauge\ncan_runs_running {running}\n"
        ));
        out.push_str(&format!(
            "# TYPE can_approvals_pending gauge\ncan_approvals_pending {pending}\n"
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_show_up_in_the_text() {
        let metrics = Metrics::default();
        metrics.run_started();
        metrics.run_started();
        metrics.approval_resolved();
        metrics.run_finished("completed", "acme", 120);
        metrics.run_finished("completed", "acme", 30);
        metrics.run_finished("cancelled", "globex", 5);
        assert_eq!(metrics.tokens_used("acme"), 150);
        assert_eq!(metrics.tokens_used("fremd"), 0);
        let text = metrics.render(3, 1);
        for line in [
            "can_runs_started_total 2",
            "can_runs_finished_total{reason=\"completed\"} 2",
            "can_runs_finished_total{reason=\"cancelled\"} 1",
            "can_tokens_total{tenant=\"acme\"} 150",
            "can_approvals_resolved_total 1",
            "can_runs_running 3",
            "can_approvals_pending 1",
        ] {
            assert!(text.contains(line), "{line} fehlt in:\n{text}");
        }
    }
}
