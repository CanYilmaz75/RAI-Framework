//! Baut aus der Konfiguration eines Agenten den Agenten: Provider, Tools, Sandbox, Policy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use can_core::{
    BlockList, Limits, MemoryBank, Provider, RuleSet, Secret, Store, Tool, ToolCtx, ToolError,
    ToolOutput, ToolSpec,
};
use can_providers::{ProviderSettings, Retry};
use can_runtime::{Agent, BackgroundAgents, SubAgent, SystemClock};
use can_tools::mcp::HttpTransport;
use can_tools::{
    HttpFetch, LoadSkill, McpClient, ReadFile, SandboxedShell, SkillSet, WasmTool, WriteTodos,
    edit_file, glob, grep, load_memory, ls, memory_tools, shell, system_prompt, write_file,
};

use crate::config::{AgentConfig, ConfigError, invalid};

/// Macht aus einem Tool eines, dessen Ausgaben als vertraulich gelten.
struct Confidential {
    inner: Arc<dyn Tool>,
    spec: ToolSpec,
}

#[async_trait::async_trait]
impl Tool for Confidential {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: serde_json::Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.inner.call(args, cx).await
    }
}

impl AgentConfig {
    /// Die Anbieter-Einstellungen dieses Agenten.
    pub(crate) fn provider_settings(&self) -> ProviderSettings {
        ProviderSettings {
            kind: self.provider.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            region: self.region.clone(),
            project: self.project.clone(),
            location: self.location.clone(),
            deployment: self.deployment.clone(),
            api_version: self.api_version.clone(),
            token_env: self.token_env.clone(),
            token_command: self.token_command.clone(),
        }
    }

    fn provider(
        &self,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Arc<dyn Provider>, ConfigError> {
        let provider = self
            .provider_settings()
            .build(lookup)
            .map_err(|error| invalid(format!("Agent {}: {error}", self.name)))?;
        Ok(Arc::new(Retry::new(provider, Arc::new(SystemClock))))
    }

    pub(crate) async fn build(
        &self,
        store: Arc<dyn Store>,
        memory_bank: &Arc<dyn MemoryBank>,
        worker: &str,
        lookup: &impl Fn(&str) -> Option<String>,
        built: &HashMap<String, Agent>,
    ) -> Result<Agent, ConfigError> {
        let defaults = Limits::default();
        let limits = Limits {
            max_steps: self.max_steps.unwrap_or(defaults.max_steps),
            max_tokens: self.max_tokens.unwrap_or(defaults.max_tokens),
            timeout: self
                .timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(defaults.timeout),
            ..defaults
        };
        let mut policy = RuleSet::safe_defaults();
        policy.rules.extend(self.rules.iter().cloned());
        let mut builder = Agent::builder()
            .provider_arc(self.provider(lookup)?)
            .model(&self.model)
            .policy(policy)
            .limits(limits)
            .store_arc(store)
            .owner(worker);
        let failed = |error: std::io::Error| invalid(format!("Agent {}: {error}", self.name));
        let memory = match &self.memory_file {
            Some(path) => load_memory(path).map_err(failed)?,
            None => None,
        };
        let skills = match &self.skills_dir {
            Some(dir) => SkillSet::load(dir).map_err(failed)?,
            None => SkillSet::default(),
        };
        if self.system.is_some() || memory.is_some() || !skills.is_empty() {
            let base = self.system.as_deref().unwrap_or("");
            builder = builder.system(system_prompt(base, memory.as_deref(), &skills));
        }
        if let Some(workdir) = &self.workdir {
            builder = builder.workdir(workdir);
        }
        if self.isolate_workdir {
            builder = builder.isolate_workdir(true);
        }
        if let Some(guard) = self.exfiltration_guard {
            builder = builder.exfiltration_guard(guard);
        }
        if !self.blocked_terms.is_empty() {
            builder = builder.moderator(BlockList::new(self.blocked_terms.iter().cloned()));
        }
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for tool in &self.tools {
            tools.push(match (tool.as_str(), &self.sandbox) {
                ("read_file", _) => Arc::new(ReadFile::new()),
                ("write_file", _) => Arc::new(write_file),
                ("edit_file", _) => Arc::new(edit_file),
                ("ls", _) => Arc::new(ls),
                ("glob", _) => Arc::new(glob),
                ("grep", _) => Arc::new(grep),
                ("write_todos", _) => Arc::new(WriteTodos::new()),
                ("shell", Some(sandbox)) => Arc::new(SandboxedShell::new(sandbox.clone())),
                ("shell", None) => Arc::new(shell),
                _ => Arc::new(HttpFetch::new(self.http_allowed_hosts.clone())),
            });
        }
        if !skills.is_empty() {
            tools.push(Arc::new(LoadSkill::new(skills)));
        }
        if self.memory {
            tools.extend(memory_tools(memory_bank.clone()));
        }
        let earlier = |name: &String| {
            built.get(name).cloned().ok_or_else(|| {
                invalid(format!(
                    "Agent {}: Sub-Agent {name} ist unbekannt oder steht erst weiter unten",
                    self.name
                ))
            })
        };
        for name in &self.subagents {
            let description =
                format!("Gibt eine Aufgabe an den Agenten {name} und wartet auf seine Antwort.");
            tools.push(Arc::new(SubAgent::new(
                name.clone(),
                description,
                earlier(name)?,
            )));
        }
        for name in &self.background_agents {
            let description = format!("arbeitet als Agent {name} selbstständig im Hintergrund");
            tools.extend(BackgroundAgents::new(name.clone(), description, earlier(name)?).tools());
        }
        for (name, server) in &self.mcp {
            let client = match (&server.command, &server.url) {
                (Some(command), None) => {
                    McpClient::connect_stdio(name.clone(), command, &server.args).await
                }
                (None, Some(url)) => {
                    let mut transport = HttpTransport::new(url.clone());
                    if let Some(variable) = &server.token_env {
                        let token = lookup(variable)
                            .ok_or_else(|| invalid(format!("Variable {variable} fehlt")))?;
                        transport = transport.with_bearer(Secret::new(token));
                    }
                    McpClient::connect(name.clone(), transport).await
                }
                _ => {
                    return Err(invalid(format!(
                        "MCP-Server {name}: genau eines von `command` und `url` angeben"
                    )));
                }
            };
            let client = client.map_err(|error| invalid(format!("MCP-Server {name}: {error}")))?;
            tools.extend(
                client
                    .tools()
                    .await
                    .map_err(|error| invalid(format!("MCP-Server {name}: {error}")))?,
            );
        }
        for wasm in &self.wasm_tools {
            let tool = WasmTool::load(wasm.clone()).map_err(|error| {
                invalid(format!(
                    "Agent {}, WASM-Tool {}: {error}",
                    self.name, wasm.name
                ))
            })?;
            tools.push(Arc::new(tool));
        }
        for tool in tools {
            let spec = tool.spec().clone();
            builder = if self.confidential_tools.contains(&spec.name) {
                builder.tool(Confidential {
                    inner: tool,
                    spec: spec.with_confidential(true),
                })
            } else {
                builder.tool_arc(tool)
            };
        }
        builder
            .build()
            .map_err(|error| invalid(format!("Agent {}: {error}", self.name)))
    }
}
