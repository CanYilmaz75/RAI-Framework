//! Kennungen als eigene Newtypes über ULIDs: zeitlich sortierbar und nicht verwechselbar.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Fehler beim Lesen einer Kennung aus Text.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("ungültige Kennung: {0}")]
pub struct IdError(pub String);

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Ulid);

        impl $name {
            /// Erzeugt eine neue, zufällige Kennung mit aktuellem Zeitanteil.
            pub fn new() -> Self {
                Self(Ulid::new())
            }

            /// Baut eine Kennung aus festen Teilen, für reproduzierbare Tests und Fixtures.
            pub fn from_parts(millis: u64, random: u128) -> Self {
                Self(Ulid::from_parts(millis, random))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ulid::from_string(s).map(Self).map_err(|_| IdError(s.to_owned()))
            }
        }
    };
}

id_type!(
    /// Kennung einer Sitzung, also eines Event-Logs.
    SessionId
);
id_type!(
    /// Kennung eines einzelnen Laufs innerhalb einer Sitzung.
    RunId
);
id_type!(
    /// Kennung eines Tool-Aufrufs.
    CallId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_distinct() {
        assert_ne!(SessionId::new(), SessionId::new());
        assert_ne!(CallId::default(), CallId::default());
    }

    #[test]
    fn display_and_parse_roundtrip() {
        let id = RunId::from_parts(42, 7);
        let parsed: RunId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!("nope".parse::<CallId>(), Err(IdError("nope".into())));
    }

    #[test]
    fn serde_uses_plain_string() {
        let id = CallId::from_parts(1, 2);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<CallId>(&json).unwrap(), id);
    }

    #[test]
    fn ids_sort_by_time() {
        assert!(SessionId::from_parts(1, 9) < SessionId::from_parts(2, 0));
    }
}
