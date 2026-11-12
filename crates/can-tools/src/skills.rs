//! Gedächtnis und Skills als Dateien.
//!
//! Das Gedächtnis ist eine Datei, die immer in den System-Prompt kommt. Skills sind Ordner mit
//! einer `SKILL.md`; in den Prompt kommen nur Name und Beschreibung, der volle Text erst, wenn
//! das Modell ihn über `load_skill` anfordert. So bleibt der Kontext klein.

use std::collections::BTreeMap;
use std::path::Path;

use can_core::{Effect, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};

/// Größte Gedächtnis- oder Skill-Datei, die geladen wird.
const MAX_BYTES: u64 = 256 * 1024;

/// Ein Skill: Kurzangaben aus dem Kopf der Datei und ihr voller Text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Liest den Kopf zwischen den `---`-Zeilen: `name:` und `description:`.
fn parse(text: &str, fallback_name: &str) -> Skill {
    let mut name = fallback_name.to_owned();
    let mut description = String::new();
    let mut body = text;
    if let Some(rest) = text.strip_prefix("---")
        && let Some((head, tail)) = rest.split_once("\n---")
    {
        for line in head.lines() {
            match line.split_once(':') {
                Some(("name", value)) if !value.trim().is_empty() => name = value.trim().to_owned(),
                Some(("description", value)) => description = value.trim().to_owned(),
                _ => {}
            }
        }
        body = tail.trim_start_matches(['\r', '\n']);
    }
    Skill {
        name,
        description,
        body: body.trim_end().to_owned(),
    }
}

fn read_limited(path: &Path) -> std::io::Result<String> {
    if std::fs::metadata(path)?.len() > MAX_BYTES {
        return Err(std::io::Error::other(format!(
            "{} ist größer als {MAX_BYTES} Bytes",
            path.display()
        )));
    }
    std::fs::read_to_string(path)
}

/// Liest die Gedächtnis-Datei. Fehlt sie, gibt es kein Gedächtnis; das ist kein Fehler.
pub fn load_memory(path: &Path) -> std::io::Result<Option<String>> {
    match read_limited(path) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Alle Skills eines Ordners, nach Name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillSet {
    skills: BTreeMap<String, Skill>,
}

impl SkillSet {
    /// Liest jeden Unterordner von `dir`, der eine `SKILL.md` enthält. Ein fehlender Ordner
    /// ergibt eine leere Menge.
    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let mut skills = BTreeMap::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error),
        };
        for entry in entries.flatten() {
            let file = entry.path().join("SKILL.md");
            if !file.is_file() {
                continue;
            }
            let folder = entry.file_name().to_string_lossy().into_owned();
            let skill = parse(&read_limited(&file)?, &folder);
            skills.insert(skill.name.clone(), skill);
        }
        Ok(Self { skills })
    }

    /// Gibt es keinen Skill?
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Namen aller Skills.
    pub fn names(&self) -> Vec<&str> {
        self.skills.keys().map(String::as_str).collect()
    }

    /// Der volle Skill nach Name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }
}

/// Setzt den System-Prompt zusammen: der eigene Prompt, dann das Gedächtnis, dann die Liste
/// der Skills mit Name und Beschreibung.
pub fn system_prompt(base: &str, memory: Option<&str>, skills: &SkillSet) -> String {
    let mut prompt = base.trim().to_owned();
    if let Some(memory) = memory {
        prompt.push_str("\n\n# Gedächtnis\n\nDas Folgende gilt dauerhaft für dieses Projekt:\n\n");
        prompt.push_str(memory);
    }
    if !skills.is_empty() {
        prompt.push_str("\n\n# Skills\n\nFür diese Aufgaben gibt es Anleitungen. Lade eine mit `load_skill`, bevor du sie brauchst:\n");
        for skill in skills.skills.values() {
            prompt.push_str(&format!("\n- {}: {}", skill.name, skill.description));
        }
    }
    prompt
}

/// Tool `load_skill`: liefert den vollen Text eines Skills.
pub struct LoadSkill {
    spec: ToolSpec,
    skills: SkillSet,
}

impl LoadSkill {
    /// Tool über die gegebenen Skills.
    pub fn new(skills: SkillSet) -> Self {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string", "enum": skills.names()}},
            "required": ["name"],
            "additionalProperties": false
        });
        let spec = ToolSpec::new(
            "load_skill",
            "Lädt die vollständige Anleitung eines Skills.",
            schema,
        )
        .with_effect(Effect::ReadOnly)
        .with_idempotent(true);
        Self { spec, skills }
    }
}

#[async_trait::async_trait]
impl Tool for LoadSkill {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let name = args["name"].as_str().unwrap_or("");
        match self.skills.get(name) {
            Some(skill) => Ok(ToolOutput::text(skill.body.clone())),
            None => Err(ToolError::ForModel(format!(
                "Unbekannter Skill: {name}. Vorhanden: {}",
                self.skills.names().join(", ")
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;
    use can_core::schema::validate;

    fn skills_dir() -> TempDir {
        let dir = TempDir::new();
        dir.write(
            "skills/release/SKILL.md",
            b"---\nname: release\ndescription: Wie eine Version ver\xc3\xb6ffentlicht wird\n---\n\n1. Tests laufen lassen\n2. Tag setzen\n",
        );
        dir.write(
            "skills/review/SKILL.md",
            b"Ohne Kopf: achte auf Fehlerbehandlung.\n",
        );
        dir.write("skills/leer/notiz.txt", b"kein Skill");
        dir
    }

    #[test]
    fn skills_load_with_and_without_a_header() {
        let dir = skills_dir();
        let skills = SkillSet::load(&dir.path().join("skills")).unwrap();
        assert_eq!(skills.names(), ["release", "review"]);
        let release = skills.get("release").unwrap();
        assert_eq!(release.description, "Wie eine Version veröffentlicht wird");
        assert_eq!(release.body, "1. Tests laufen lassen\n2. Tag setzen");
        assert_eq!(
            skills.get("review").unwrap().body,
            "Ohne Kopf: achte auf Fehlerbehandlung."
        );
        assert!(
            SkillSet::load(&dir.path().join("fehlt"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn prompt_carries_memory_in_full_but_only_skill_summaries() {
        let dir = skills_dir();
        dir.write("AGENTS.md", b"Antworte auf Deutsch.\nNutze cargo fmt.\n");
        let skills = SkillSet::load(&dir.path().join("skills")).unwrap();
        let memory = load_memory(&dir.path().join("AGENTS.md")).unwrap();
        let prompt = system_prompt("Du hilfst.", memory.as_deref(), &skills);
        assert!(prompt.starts_with("Du hilfst.\n\n# Gedächtnis"));
        assert!(prompt.contains("Antworte auf Deutsch.\nNutze cargo fmt."));
        assert!(prompt.contains("- release: Wie eine Version veröffentlicht wird"));
        assert!(
            !prompt.contains("Tag setzen"),
            "der volle Skill kommt erst auf Anforderung"
        );
        assert_eq!(
            system_prompt("Du hilfst.", None, &SkillSet::default()),
            "Du hilfst."
        );
    }

    #[test]
    fn missing_or_empty_memory_is_no_memory() {
        let dir = TempDir::new();
        assert_eq!(load_memory(&dir.path().join("AGENTS.md")).unwrap(), None);
        dir.write("AGENTS.md", b"  \n");
        assert_eq!(load_memory(&dir.path().join("AGENTS.md")).unwrap(), None);
        dir.write("gross.md", &vec![b'x'; 300_000]);
        assert!(load_memory(&dir.path().join("gross.md")).is_err());
    }

    #[tokio::test]
    async fn load_skill_returns_the_full_text() {
        let dir = skills_dir();
        let tool = LoadSkill::new(SkillSet::load(&dir.path().join("skills")).unwrap());
        assert_eq!(tool.spec().effect, Effect::ReadOnly);
        assert!(
            validate(
                &tool.spec().input_schema.0,
                &json!({"name": "gibt es nicht"})
            )
            .is_err()
        );
        let text = tool
            .call(json!({"name": "release"}), &dir.ctx())
            .await
            .unwrap()
            .as_text();
        assert_eq!(text, "1. Tests laufen lassen\n2. Tag setzen");
        let unknown = tool.call(json!({"name": "x"}), &dir.ctx()).await;
        assert!(matches!(unknown, Err(ToolError::ForModel(m)) if m.contains("release, review")));
    }
}
