//! Durchsuchbares Gedächtnis über Sitzungen hinweg.
//!
//! Einträge gehören zu einem Namensraum, etwa einem Mandanten oder einem Nutzer. Die Suche ist
//! eine Volltextsuche mit TF-IDF-Gewichtung: ohne Einbettungsmodell, ohne weitere Dienste, und
//! für jedes Speicher-Backend gleich, weil die Rangfolge hier berechnet wird.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::event::Timestamp;
use crate::store::StoreError;

/// Namensraum, wenn nichts anderes gesetzt ist.
pub const DEFAULT_NAMESPACE: &str = "default";

/// Ein gespeicherter Eintrag, bei Suchergebnissen mit Relevanz.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub text: String,
    pub created_at: Timestamp,
    /// Relevanz für die Suche; 0 außerhalb einer Suche.
    #[serde(default)]
    pub score: f64,
}

/// Ablage für Gedächtniseinträge.
#[async_trait]
pub trait MemoryBank: Send + Sync {
    /// Speichert einen Eintrag und liefert seine Kennung.
    async fn add(&self, namespace: &str, text: &str, now: Timestamp) -> Result<String, StoreError>;

    /// Alle Einträge des Namensraums, älteste zuerst.
    async fn all(&self, namespace: &str) -> Result<Vec<Memory>, StoreError>;

    /// Löscht einen Eintrag; `false`, wenn es ihn nicht gab.
    async fn delete(&self, namespace: &str, id: &str) -> Result<bool, StoreError>;

    /// Die relevantesten Einträge zur Anfrage, beste zuerst.
    async fn search(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Memory>, StoreError> {
        Ok(rank(self.all(namespace).await?, query, limit))
    }
}

/// Zerlegt Text in kleingeschriebene Wörter aus Buchstaben und Ziffern.
pub fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Ordnet Einträge nach TF-IDF-Relevanz zur Anfrage. Einträge ohne gemeinsames Wort fallen weg;
/// bei gleicher Relevanz gewinnt der neuere.
pub fn rank(memories: Vec<Memory>, query: &str, limit: usize) -> Vec<Memory> {
    let query: HashSet<String> = tokens(query).into_iter().collect();
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }
    let documents: Vec<Vec<String>> = memories.iter().map(|m| tokens(&m.text)).collect();
    let total = documents.len() as f64;
    let mut frequency: HashMap<&str, f64> = HashMap::new();
    for words in &documents {
        let unique: HashSet<&str> = words.iter().map(String::as_str).collect();
        for word in unique.into_iter().filter(|word| query.contains(*word)) {
            *frequency.entry(word).or_default() += 1.0;
        }
    }
    let mut scored: Vec<Memory> = memories
        .into_iter()
        .zip(&documents)
        .filter_map(|(mut memory, words)| {
            let length = words.len().max(1) as f64;
            let score: f64 = query
                .iter()
                .map(|term| {
                    let count = words.iter().filter(|word| *word == term).count() as f64;
                    let df = frequency.get(term.as_str()).copied().unwrap_or(0.0);
                    (count / length) * (1.0 + (total + 1.0) / (df + 1.0)).ln()
                })
                .sum();
            (score > 0.0).then(|| {
                memory.score = score;
                memory
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.created_at.cmp(&a.created_at))
    });
    scored.truncate(limit);
    scored
}

/// Gedächtnis im Arbeitsspeicher, für Tests und kurzlebige Prozesse.
#[derive(Default)]
pub struct InMemoryBank {
    entries: Mutex<Vec<(String, Memory)>>,
    next: Mutex<u64>,
}

impl InMemoryBank {
    /// Leeres Gedächtnis.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, StoreError> {
        mutex
            .lock()
            .map_err(|_| StoreError::Backend("Sperre des Gedächtnisses ist vergiftet".into()))
    }
}

#[async_trait]
impl MemoryBank for InMemoryBank {
    async fn add(&self, namespace: &str, text: &str, now: Timestamp) -> Result<String, StoreError> {
        let id = {
            let mut next = Self::lock(&self.next)?;
            *next += 1;
            format!("m{next}")
        };
        let memory = Memory {
            id: id.clone(),
            text: text.to_owned(),
            created_at: now,
            score: 0.0,
        };
        Self::lock(&self.entries)?.push((namespace.to_owned(), memory));
        Ok(id)
    }

    async fn all(&self, namespace: &str) -> Result<Vec<Memory>, StoreError> {
        Ok(Self::lock(&self.entries)?
            .iter()
            .filter(|(ns, _)| ns == namespace)
            .map(|(_, memory)| memory.clone())
            .collect())
    }

    async fn delete(&self, namespace: &str, id: &str) -> Result<bool, StoreError> {
        let mut entries = Self::lock(&self.entries)?;
        let before = entries.len();
        entries.retain(|(ns, memory)| !(ns == namespace && memory.id == id));
        Ok(entries.len() != before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: &str, text: &str, at: u64) -> Memory {
        Memory {
            id: id.into(),
            text: text.into(),
            created_at: Timestamp(at),
            score: 0.0,
        }
    }

    #[test]
    fn tokens_are_lowercase_words() {
        assert_eq!(
            tokens("Hallo, Welt! 42-mal"),
            ["hallo", "welt", "42", "mal"]
        );
        assert!(tokens(" ,; ").is_empty());
    }

    #[test]
    fn rank_prefers_rare_and_dense_matches() {
        let memories = vec![
            memory("a", "Der Kunde mag Tee", 1),
            memory("b", "Der Kunde heißt Müller", 2),
            memory("c", "Der Server läuft in Frankfurt", 3),
        ];
        let found = rank(memories.clone(), "Wie heißt der Kunde?", 5);
        let ids: Vec<&str> = found.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids[0], "b");
        assert!(found.iter().all(|m| m.score > 0.0));
        assert!(rank(memories.clone(), "Pinguin", 5).is_empty());
        assert!(rank(memories.clone(), "", 5).is_empty());
        assert_eq!(rank(memories, "der", 1).len(), 1);
    }

    #[test]
    fn equal_scores_prefer_newer_entries() {
        let found = rank(
            vec![memory("alt", "Tee", 1), memory("neu", "Tee", 9)],
            "tee",
            2,
        );
        assert_eq!(found[0].id, "neu");
    }

    #[tokio::test]
    async fn in_memory_bank_separates_namespaces() {
        let bank = InMemoryBank::new();
        let id = bank
            .add("a", "Lieblingsfarbe ist Blau", Timestamp(1))
            .await
            .unwrap();
        bank.add("b", "Lieblingsfarbe ist Rot", Timestamp(2))
            .await
            .unwrap();
        let found = bank.search("a", "Lieblingsfarbe", 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "Lieblingsfarbe ist Blau");
        assert_eq!(bank.all("b").await.unwrap().len(), 1);
        assert!(!bank.delete("b", &id).await.unwrap());
        assert!(bank.delete("a", &id).await.unwrap());
        assert!(bank.all("a").await.unwrap().is_empty());
    }
}
