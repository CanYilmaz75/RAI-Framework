//! Anmeldung über API-Schlüssel und Rollen. Gespeichert ist nur die Prüfsumme eines Schlüssels.

use std::collections::HashMap;

use can_core::Digest;
use serde::{Deserialize, Serialize};

/// Was ein Nutzer darf. Die Ordnung ist der Umfang: `Viewer < Operator < Admin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Darf lesen.
    Viewer,
    /// Darf zusätzlich Läufe starten, abbrechen und Freigaben erteilen.
    Operator,
    /// Darf zusätzlich alles im eigenen Mandanten verwalten.
    Admin,
}

/// Wer eine Anfrage stellt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub tenant: String,
    pub user: String,
    pub role: Role,
}

/// Grenzen eines Mandanten.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quota {
    /// Höchstzahl gleichzeitig laufender Läufe.
    #[serde(default = "Quota::default_runs")]
    pub max_concurrent_runs: usize,
    /// Token-Budget des Mandanten, seit der Dienst läuft; null heißt unbegrenzt.
    #[serde(default)]
    pub max_tokens: u64,
}

impl Quota {
    fn default_runs() -> usize {
        8
    }
}

impl Default for Quota {
    fn default() -> Self {
        Self {
            max_concurrent_runs: Self::default_runs(),
            max_tokens: 0,
        }
    }
}

/// Alle bekannten Schlüssel und Mandanten.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    keys: HashMap<Digest, Principal>,
    quotas: HashMap<String, Quota>,
}

impl Directory {
    /// Leeres Verzeichnis: niemand kommt hinein.
    pub fn new() -> Self {
        Self::default()
    }

    /// Legt einen Mandanten mit seinen Grenzen an.
    pub fn tenant(mut self, name: impl Into<String>, quota: Quota) -> Self {
        self.quotas.insert(name.into(), quota);
        self
    }

    /// Trägt einen Schlüssel ein. Behalten wird nur seine Prüfsumme.
    pub fn key(
        mut self,
        key: &str,
        tenant: impl Into<String>,
        user: impl Into<String>,
        role: Role,
    ) -> Self {
        let tenant = tenant.into();
        self.quotas.entry(tenant.clone()).or_default();
        self.keys.insert(
            Digest::of(key.as_bytes()),
            Principal {
                tenant,
                user: user.into(),
                role,
            },
        );
        self
    }

    /// Wer steckt hinter dem Wert des Headers `Authorization`?
    pub fn authenticate(&self, header: Option<&str>) -> Option<Principal> {
        let key = header?.strip_prefix("Bearer ")?.trim();
        self.keys.get(&Digest::of(key.as_bytes())).cloned()
    }

    /// Grenzen des Mandanten.
    pub fn quota(&self, tenant: &str) -> Quota {
        self.quotas.get(tenant).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_principals_and_wrong_keys_to_nobody() {
        let directory = Directory::new()
            .tenant(
                "acme",
                Quota {
                    max_concurrent_runs: 2,
                    max_tokens: 1_000,
                },
            )
            .key("geheim-1", "acme", "ada", Role::Admin)
            .key("geheim-2", "globex", "bob", Role::Viewer);
        let ada = directory.authenticate(Some("Bearer geheim-1")).unwrap();
        assert_eq!(
            (ada.tenant.as_str(), ada.user.as_str(), ada.role),
            ("acme", "ada", Role::Admin)
        );
        assert_eq!(
            directory
                .authenticate(Some("Bearer geheim-2"))
                .unwrap()
                .tenant,
            "globex"
        );
        for header in [
            None,
            Some("Bearer falsch"),
            Some("geheim-1"),
            Some("Basic geheim-1"),
            Some(""),
        ] {
            assert_eq!(directory.authenticate(header), None, "{header:?}");
        }
        assert_eq!(directory.quota("acme").max_concurrent_runs, 2);
        assert_eq!(directory.quota("globex"), Quota::default());
        assert!(
            !format!("{directory:?}").contains("geheim-1"),
            "Schlüssel stehen nicht im Klartext"
        );
    }

    #[test]
    fn roles_are_ordered_by_scope() {
        assert!(Role::Viewer < Role::Operator && Role::Operator < Role::Admin);
    }
}
