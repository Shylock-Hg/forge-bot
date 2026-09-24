//! Agent registry.
//!
//! Holds every configured [`Agent`] and resolves the adapter to use for a
//! request. The four built-ins (Codex, Pi, Claude Code, Kimi) are always
//! available and can be overridden or extended from configuration.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::agent::command::CommandAgent;
use crate::agent::{Agent, claude, codex, kimi, pi};
use crate::config::Config;
use crate::error::{BotError, Result};

/// Names of the adapters that are always registered.
pub const BUILTIN_AGENTS: &[&str] = &["codex", "pi", "claude", "kimi"];

/// Resolves agent names to adapters.
pub struct AgentRegistry {
    agents: BTreeMap<String, Arc<dyn Agent>>,
    default: String,
}

impl AgentRegistry {
    /// Build the registry from configuration.
    pub fn from_config(config: &Config) -> Self {
        let mut agents: BTreeMap<String, Arc<dyn Agent>> = BTreeMap::new();

        let overrides = &config.agents.overrides;

        let codex_cfg = overrides.get("codex").cloned().unwrap_or_default();
        agents.insert("codex".into(), Arc::new(codex::build(&codex_cfg)));

        let pi_cfg = overrides.get("pi").cloned().unwrap_or_default();
        agents.insert("pi".into(), Arc::new(pi::build(&pi_cfg)));

        let claude_cfg = overrides.get("claude").cloned().unwrap_or_default();
        agents.insert("claude".into(), Arc::new(claude::build(&claude_cfg)));

        let kimi_cfg = overrides.get("kimi").cloned().unwrap_or_default();
        agents.insert("kimi".into(), Arc::new(kimi::build(&kimi_cfg)));

        // Custom adapters: any override that is not a built-in must provide a
        // command to run.
        for (name, cfg) in overrides {
            if agents.contains_key(name) {
                continue;
            }
            match &cfg.command {
                Some(command) if !command.is_empty() => {
                    let agent = CommandAgent::new(name.clone(), command).apply_config(cfg);
                    agents.insert(name.clone(), Arc::new(agent));
                }
                _ => {
                    tracing::warn!(
                        agent = %name,
                        "custom agent has no `command` configured; skipping"
                    );
                }
            }
        }

        let default = if agents.contains_key(&config.default_agent) {
            config.default_agent.clone()
        } else if !config.agents.default.is_empty() && agents.contains_key(&config.agents.default) {
            config.agents.default.clone()
        } else {
            agents
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "codex".to_owned())
        };

        Self { agents, default }
    }

    /// The configured default agent name.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// Look up an agent by name.
    pub fn get(&self, name: &str) -> Result<Arc<dyn Agent>> {
        self.agents
            .get(name)
            .cloned()
            .ok_or_else(|| BotError::UnknownAgent(name.to_owned()))
    }

    /// Resolve an explicit name or fall back to the default.
    pub fn resolve(&self, name: Option<&str>) -> Result<Arc<dyn Agent>> {
        match name {
            Some(name) if !name.is_empty() => self.get(name),
            _ => self.get(&self.default),
        }
    }

    /// All registered agent names.
    pub fn names(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRegistry")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .field("default", &self.default)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;

    #[test]
    fn registers_builtins() {
        let registry = AgentRegistry::from_config(&Config::default());
        for name in BUILTIN_AGENTS {
            assert!(registry.names().contains(&name.to_string()));
            assert_eq!(registry.get(name).unwrap().name(), *name);
        }
    }

    #[test]
    fn custom_agent_requires_command() {
        let mut config = Config::default();
        config
            .agents
            .overrides
            .insert("custom".into(), AgentConfig::default());
        let registry = AgentRegistry::from_config(&config);
        assert!(registry.get("custom").is_err());

        config.agents.overrides.insert(
            "custom".into(),
            AgentConfig {
                command: Some("my-agent".into()),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config(&config);
        assert_eq!(registry.get("custom").unwrap().name(), "custom");
    }

    #[test]
    fn unknown_agent_is_reported() {
        let registry = AgentRegistry::from_config(&Config::default());
        assert!(matches!(
            registry.get("does-not-exist"),
            Err(BotError::UnknownAgent(_))
        ));
    }
}
