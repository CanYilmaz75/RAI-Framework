//! Inhaltsfilter: prüft Eingaben der Nutzer und Antworten des Modells.

/// Entscheidet, ob ein Text durchgelassen wird. Der Fehler nennt den Grund.
pub trait Moderator: Send + Sync {
    fn check(&self, text: &str) -> Result<(), String>;
}

/// Einfacher Filter über verbotene Begriffe, ohne Beachtung der Groß- und Kleinschreibung.
/// Für mehr lässt sich hinter dem Trait ein externer Moderationsdienst anschließen.
#[derive(Clone, Debug, Default)]
pub struct BlockList {
    terms: Vec<String>,
}

impl BlockList {
    /// Filter mit den angegebenen Begriffen.
    pub fn new<I, S>(terms: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let terms = terms
            .into_iter()
            .map(|term| term.into().to_lowercase())
            .filter(|term| !term.is_empty());
        Self {
            terms: terms.collect(),
        }
    }
}

impl Moderator for BlockList {
    fn check(&self, text: &str) -> Result<(), String> {
        let lower = text.to_lowercase();
        match self.terms.iter().find(|term| lower.contains(term.as_str())) {
            Some(term) => Err(format!("enthält den gesperrten Begriff „{term}“")),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_list_matches_case_insensitively() {
        let filter = BlockList::new(["Projekt Zeus", ""]);
        assert_eq!(filter.check("alles gut"), Ok(()));
        assert!(
            filter
                .check("Was ist PROJEKT ZEUS?")
                .unwrap_err()
                .contains("projekt zeus")
        );
        assert_eq!(BlockList::default().check("Projekt Zeus"), Ok(()));
    }
}
