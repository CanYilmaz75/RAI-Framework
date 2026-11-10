//! Tool-Registry: Namen sind eindeutig, Kollisionen sind ein Fehler beim Bauen.

use std::collections::BTreeMap;
use std::sync::Arc;

use can_core::{Tool, ToolSpec};

/// Alle Tools eines Agenten, nach Name.
#[derive(Clone, Default)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl Registry {
    /// Leere Registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trägt ein Tool ein. Ein schon vergebener Name ist ein Fehler.
    pub fn insert(&mut self, tool: Arc<dyn Tool>) -> Result<(), String> {
        let name = tool.spec().name.clone();
        if self.tools.contains_key(&name) {
            return Err(format!("Tool-Name doppelt vergeben: {name}"));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Tool nach Name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Specs aller Tools, alphabetisch nach Name.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|tool| tool.spec().clone())
            .collect()
    }

    /// Anzahl der Tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Ist die Registry leer?
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::{ToolCtx, ToolError, ToolOutput};
    use serde_json::{Value, json};

    struct Named(ToolSpec);

    #[async_trait::async_trait]
    impl Tool for Named {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }

        async fn call(&self, _args: Value, _cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn tool(name: &str) -> Arc<dyn Tool> {
        Arc::new(Named(ToolSpec::new(name, "d", json!({}))))
    }

    #[test]
    fn insert_get_and_sorted_specs() {
        let mut registry = Registry::new();
        assert!(registry.is_empty());
        registry.insert(tool("b")).unwrap();
        registry.insert(tool("a")).unwrap();
        assert_eq!(registry.len(), 2);
        assert!(registry.get("a").is_some());
        assert!(registry.get("c").is_none());
        let names: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let mut registry = Registry::new();
        registry.insert(tool("a")).unwrap();
        assert_eq!(
            registry.insert(tool("a")),
            Err("Tool-Name doppelt vergeben: a".to_owned())
        );
        assert_eq!(registry.len(), 1);
    }
}
