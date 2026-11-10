//! Secrets: ein Typ ohne `Debug` und `Serialize`, damit Werte nicht versehentlich abfließen.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Hüllt einen geheimen Wert ein. Bewusst ohne `Debug`, `Display` und `Serialize`.
#[derive(Clone)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Verpackt einen geheimen Wert.
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Gibt den Wert ausdrücklich heraus.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

/// Die für ein Tool ausdrücklich freigegebenen Schlüssel.
#[derive(Clone, Default)]
pub struct SecretsHandle(Arc<HashMap<String, Secret<String>>>);

impl SecretsHandle {
    /// Handle ohne freigegebene Schlüssel.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Handle mit genau den übergebenen Schlüsseln.
    pub fn from_pairs<I, K>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, Secret<String>)>,
        K: Into<String>,
    {
        Self(Arc::new(
            pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        ))
    }

    /// Wert eines freigegebenen Schlüssels.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|secret| secret.expose().as_str())
    }
}

impl fmt::Debug for SecretsHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys: Vec<&String> = self.0.keys().collect();
        keys.sort();
        f.debug_struct("SecretsHandle")
            .field("keys", &keys)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_returns_only_granted_keys() {
        let handle = SecretsHandle::from_pairs([("API_KEY", Secret::new("geheim".to_owned()))]);
        assert_eq!(handle.get("API_KEY"), Some("geheim"));
        assert_eq!(handle.get("ANDERER"), None);
        assert_eq!(SecretsHandle::empty().get("API_KEY"), None);
    }

    #[test]
    fn debug_shows_keys_but_never_values() {
        let handle = SecretsHandle::from_pairs([("API_KEY", Secret::new("geheim".to_owned()))]);
        let debug = format!("{handle:?}");
        assert!(debug.contains("API_KEY"));
        assert!(!debug.contains("geheim"));
    }

    #[test]
    fn secret_exposes_value_on_request() {
        assert_eq!(*Secret::new(7).expose(), 7);
    }
}
