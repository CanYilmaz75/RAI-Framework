//! Anbieter mit OpenAI-kompatibler Schnittstelle: Basis-URL und Umgebungsvariable des Schlüssels.

/// Ein bekannter Anbieter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub base_url: &'static str,
    /// Variable, aus der der Schlüssel kommt; `None`, wenn keiner nötig ist.
    pub key_env: Option<&'static str>,
}

/// Alle bekannten OpenAI-kompatiblen Anbieter.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "openai",
        base_url: "https://api.openai.com/v1",
        key_env: Some("OPENAI_API_KEY"),
    },
    Preset {
        name: "ollama",
        base_url: "http://localhost:11434/v1",
        key_env: None,
    },
    Preset {
        name: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        key_env: Some("OPENROUTER_API_KEY"),
    },
    Preset {
        name: "groq",
        base_url: "https://api.groq.com/openai/v1",
        key_env: Some("GROQ_API_KEY"),
    },
    Preset {
        name: "mistral",
        base_url: "https://api.mistral.ai/v1",
        key_env: Some("MISTRAL_API_KEY"),
    },
    Preset {
        name: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        key_env: Some("DEEPSEEK_API_KEY"),
    },
    Preset {
        name: "xai",
        base_url: "https://api.x.ai/v1",
        key_env: Some("XAI_API_KEY"),
    },
    Preset {
        name: "together",
        base_url: "https://api.together.xyz/v1",
        key_env: Some("TOGETHER_API_KEY"),
    },
    Preset {
        name: "fireworks",
        base_url: "https://api.fireworks.ai/inference/v1",
        key_env: Some("FIREWORKS_API_KEY"),
    },
    Preset {
        name: "perplexity",
        base_url: "https://api.perplexity.ai",
        key_env: Some("PERPLEXITY_API_KEY"),
    },
];

/// Voreinstellung nach Name.
pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_unique_and_use_https_except_local_ollama() {
        let mut names: Vec<&str> = PRESETS.iter().map(|preset| preset.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), PRESETS.len());
        for preset in PRESETS {
            let local = preset.name == "ollama";
            assert_eq!(
                preset.base_url.starts_with("https://"),
                !local,
                "{}",
                preset.name
            );
            assert_eq!(preset.key_env.is_none(), local, "{}", preset.name);
        }
        assert_eq!(
            preset("groq").map(|p| p.key_env),
            Some(Some("GROQ_API_KEY"))
        );
        assert!(preset("gibt-es-nicht").is_none());
    }
}
