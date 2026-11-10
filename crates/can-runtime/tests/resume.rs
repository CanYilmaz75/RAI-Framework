//! Absturzfestigkeit, Wiederaufnahme und Record/Replay.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use can_core::{
    AllowAll, CallId, Capabilities, Chunk, Envelope, FinishReason, ManualClock, MemoryStore,
    Provider, ProviderError, ProviderStream, Request, Role, SessionId, StopReason, Store,
    StoreError, Timestamp, Usage,
};
use can_providers::{FakeProvider, Recording};
use can_runtime::{Agent, RunError};
use common::{TestTool, drain};

/// Store, der nach einer festen Zahl von Events jeden weiteren Schreibversuch verweigert:
/// so sieht ein Absturz zwischen zwei Events aus Sicht des Logs aus.
struct CrashingStore {
    inner: Arc<MemoryStore>,
    budget: AtomicUsize,
}

#[async_trait]
impl Store for CrashingStore {
    async fn load(&self, id: SessionId) -> Result<Vec<Envelope>, StoreError> {
        self.inner.load(id).await
    }

    async fn append(&self, id: SessionId, events: &[Envelope]) -> Result<(), StoreError> {
        let left = self.budget.load(Ordering::SeqCst);
        if events.len() > left {
            self.budget.store(0, Ordering::SeqCst);
            return Err(StoreError::Backend("simulierter Absturz".into()));
        }
        self.budget.store(left - events.len(), Ordering::SeqCst);
        self.inner.append(id, events).await
    }
    async fn put_blob(&self, bytes: Vec<u8>) -> Result<can_core::Digest, StoreError> {
        self.inner.put_blob(bytes).await
    }

    async fn get_blob(&self, digest: &can_core::Digest) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_blob(digest).await
    }

    async fn acquire(
        &self,
        id: SessionId,
        owner: &str,
        now: Timestamp,
        ttl: std::time::Duration,
    ) -> Result<(), StoreError> {
        self.inner.acquire(id, owner, now, ttl).await
    }

    async fn release(&self, id: SessionId, owner: &str) -> Result<(), StoreError> {
        self.inner.release(id, owner).await
    }
}

/// Modell-Ersatz ohne eigenen Zustand: die Antwort hängt nur vom Verlauf ab, damit ein
/// fortgesetzter Lauf dieselben Antworten bekommt wie ein ununterbrochener.
struct ByHistory;

#[async_trait]
impl Provider for ByHistory {
    fn capabilities(&self, model: &str) -> Capabilities {
        FakeProvider::new().capabilities(model)
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let turn = req
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count() as u128;
        let done = |stop| Chunk::Done {
            usage: Usage::default(),
            stop,
        };
        let call = |name: &str| {
            vec![
                Chunk::ToolCallStart {
                    index: 0,
                    id: CallId::from_parts(1, turn + 1),
                    name: name.into(),
                },
                Chunk::ToolCallArgs {
                    index: 0,
                    delta: "{\"text\":\"x\"}".into(),
                },
                done(StopReason::ToolUse),
            ]
        };
        let chunks = match turn {
            0 => call("schreib"),
            1 => call("lies"),
            _ => vec![
                Chunk::TextDelta {
                    text: "Alles erledigt.".into(),
                },
                done(StopReason::EndTurn),
            ],
        };
        Ok(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))))
    }
}

struct Setup {
    agent: Agent,
    writes: Arc<AtomicUsize>,
}

fn setup(store: Arc<dyn Store>, writes: Arc<AtomicUsize>) -> Setup {
    let mut writer = TestTool::writer("schreib");
    writer.calls = writes.clone();
    let agent = Agent::builder()
        .provider(ByHistory)
        .model("test")
        .tool(writer)
        .tool(TestTool::reader("lies"))
        .policy(AllowAll)
        .store_arc(store)
        .clock(Arc::new(ManualClock::new(Timestamp(5_000))))
        .build()
        .unwrap();
    Setup { agent, writes }
}

/// Abnahme Phase 3 und Teststrategie „Absturzfestigkeit“: Der Lauf wird nach jedem einzelnen
/// Event abgebrochen und fortgesetzt. Das Ergebnis ist gleich, und das nicht idempotente Tool
/// läuft nie doppelt.
#[tokio::test]
async fn crash_after_every_event_then_resume() {
    // Vergleichslauf ohne Absturz.
    let reference = Arc::new(MemoryStore::new());
    let Setup { agent, writes } = setup(reference.clone(), Arc::new(AtomicUsize::new(0)));
    let session = SessionId::new();
    let expected = agent.complete(session, "los").await.unwrap();
    let total = reference.load(session).await.unwrap().len();
    assert_eq!(
        (expected.reason.clone(), writes.load(Ordering::SeqCst)),
        (FinishReason::Completed, 1)
    );
    assert_eq!(total, 16);

    let mut interrupted_mid_tool = 0;
    for cut in 1..total {
        let memory = Arc::new(MemoryStore::new());
        let writes = Arc::new(AtomicUsize::new(0));
        let session = SessionId::new();

        let crashing = Arc::new(CrashingStore {
            inner: memory.clone(),
            budget: AtomicUsize::new(cut),
        });
        let crashed = setup(crashing, writes.clone())
            .agent
            .complete(session, "los")
            .await;
        assert!(
            matches!(crashed, Err(RunError::Store(_))),
            "Schnitt {cut}: {crashed:?}"
        );
        let stored = memory.load(session).await.unwrap().len();
        assert!(stored <= cut, "Schnitt {cut}");
        if stored == 0 {
            // Der erste Schritt wird als Ganzes gespeichert; davor gibt es nichts fortzusetzen.
            continue;
        }

        // Neuer Prozess: frischer Agent auf demselben Log.
        let Setup { agent, writes } = setup(memory.clone(), writes);
        assert!(matches!(
            agent.run(session, "nochmal").await,
            Err(RunError::Interrupted)
        ));
        let outcome = agent
            .resume(session)
            .await
            .unwrap()
            .outcome()
            .await
            .unwrap();

        assert_eq!(outcome.reason, FinishReason::Completed, "Schnitt {cut}");
        assert_eq!(outcome.text, expected.text, "Schnitt {cut}");
        assert!(
            writes.load(Ordering::SeqCst) <= 1,
            "Schnitt {cut}: Schreib-Tool lief doppelt"
        );
        let log = memory.load(session).await.unwrap();
        assert!(
            log.iter().enumerate().all(|(i, e)| e.seq == i as u64),
            "Schnitt {cut}: seq lückenlos"
        );
        if log.iter().any(|e| {
            serde_json::to_string(&e.event)
                .unwrap()
                .contains("Unterbrochen, Ausgang unbekannt")
        }) {
            interrupted_mid_tool += 1;
        }
        assert!(matches!(
            agent.resume(session).await,
            Err(RunError::NothingToResume)
        ));
    }
    assert!(
        interrupted_mid_tool >= 1,
        "mindestens ein Schnitt lag mitten im Schreib-Tool"
    );
}

/// Abnahme Phase 3: Ein aufgezeichneter Lauf ergibt beim Replay Byte für Byte dieselben Events.
#[tokio::test]
async fn recorded_run_replays_byte_for_byte() {
    let path = std::env::temp_dir().join(format!("can-replay-{}.jsonl", SessionId::new()));
    let run_once = async |provider: Arc<dyn Provider>| {
        let agent = Agent::builder()
            .provider_arc(provider)
            .model("test")
            .tool(TestTool::writer("schreib"))
            .tool(TestTool::reader("lies"))
            .policy(AllowAll)
            .clock(Arc::new(ManualClock::new(Timestamp(5_000))))
            .build()
            .unwrap();
        let mut run = agent.run(SessionId::new(), "los").await.unwrap();
        let events = drain(&mut run).await.unwrap();
        // Die Lauf-Kennung ist pro Lauf neu; verglichen werden Position, Zeit und Event.
        events
            .iter()
            .map(|e| {
                format!(
                    "{} {} {}",
                    e.seq,
                    e.ts.0,
                    serde_json::to_string(&e.event).unwrap()
                )
            })
            .collect::<Vec<String>>()
            .join("\n")
    };

    let original = run_once(Arc::new(Recording::new(ByHistory, &path))).await;
    let replayed = run_once(Arc::new(FakeProvider::from_recording(&path).unwrap())).await;
    std::fs::remove_file(path).unwrap();

    assert!(original.contains("Alles erledigt.") && original.lines().count() == 17);
    assert_eq!(original.as_bytes(), replayed.as_bytes());
}
