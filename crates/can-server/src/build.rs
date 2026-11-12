//! Baut aus der Konfiguration eines Agenten den Agenten: Provider, Tools, Sandbox, Policy.

use std::sync::Arc;
use std::time::Duration;

use can_core::{
    BlockList, Limits, Provider, RuleSet, Secret, Store, Tool, ToolCtx, ToolError, ToolOutput,
    ToolSpec,
};
use can_providers::{Anthropic, OpenAiCompat, Retry};
use can_runtime::{Agent, SystemClock};
use can_tools::mcp::HttpTransport;
use can_tools::{HttpFetch, McpClient, ReadFile, SandboxedShell, shell, write_file};

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
    fn provider(
        &self,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Arc<dyn Provider>, ConfigError> {
        let source = |name: &str| {
            if name.ends_with("BASE_URL") && self.base_url.is_some() {
                return self.base_url.clone();
            }
            match &self.api_key_env {
                Some(variable) if name.ends_with("API_KEY") => lookup(variable),
                _ => lookup(name),
            }
        };
        let clock = Arc::new(SystemClock);
        Ok(match self.provider.as_str() {
            "anthropic" => Arc::new(Retry::new(
                Anthropic::from_lookup(source).map_err(invalid)?,
                clock,
            )),
            _ => Arc::new(Retry::new(
                OpenAiCompat::from_lookup(source).map_err(invalid)?,
                clock,
            )),
        })
    }

    pub(crate) async fn build(
        &self,
        store: Arc<dyn Store>,
        worker: &str,
        lookup: &impl Fn(&str) -> Option<String>,
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
        if let Some(system) = &self.system {
            builder = builder.system(system);
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
                ("shell", Some(sandbox)) => Arc::new(SandboxedShell::new(sandbox.clone())),
                ("shell", None) => Arc::new(shell),
                _ => Arc::new(HttpFetch::new(self.http_allowed_hosts.clone())),
            });
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
