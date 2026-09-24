//! Agent registry.
//!
//! Holds every configured [`Agent`] and resolves the adapter to use for a
//! request. The four built-ins (Codex, Pi, Claude Code, Kimi) are always
//! available and can be overridden or extended from configuration.
//!
//! Codex is the first choice: it leads [`BUILTIN_AGENTS`], the fallback order
//! returned by [`AgentRegistry::available_names`], and the default used when no
//! configured default resolves to a registered adapter.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::agent::command::CommandAgent;
use crate::agent::pi_rpc::PiPoolAgent;
use crate::agent::{Agent, claude, codex, kimi, pi};
use crate::config::Config;
use crate::error::{BotError, Result};

/// Names of the adapters that are always registered.
pub const BUILTIN_AGENTS: &[&str] = &["codex", "pi", "pi-rpc", "claude", "kimi"];

/// Resolves agent names to adapters.
///
/// The registry also remembers which agents are temporarily unavailable
/// because they reported a quota, rate or capacity error. That state is shared
/// by every clone of the [`Arc`] the dispatcher and the HTTP layer hold, so a
/// capacity hit on one job makes later jobs skip the agent too.
pub struct AgentRegistry {
    agents: BTreeMap<String, Arc<dyn Agent>>,
    default: String,
    /// Agent name -> instant at which it may be tried again.
    unavailable: Mutex<HashMap<String, Instant>>,
}

impl AgentRegistry {
    /// Build the registry from configuration.
    pub fn from_config(config: &Config) -> Self {
        let mut agents: BTreeMap<String, Arc<dyn Agent>> = BTreeMap::new();

        let overrides = &config.agents.overrides;

        // One backend session per `(agent, conversation)` so a later comment
        // in the same thread resumes the same context instead of cold-starting.
        let sessions = Arc::new(crate::agent::session::SessionStore::load(
            &crate::config::expand_tilde(&config.session.dir),
        ));

        let codex_cfg = overrides.get("codex").cloned().unwrap_or_default();
        agents.insert(
            "codex".into(),
            Arc::new(codex::build(&codex_cfg, Arc::clone(&sessions))),
        );

        let pi_cfg = overrides.get("pi").cloned().unwrap_or_default();
        agents.insert(
            "pi".into(),
            Arc::new(pi::build(&pi_cfg, Arc::clone(&sessions))),
        );

        let claude_cfg = overrides.get("claude").cloned().unwrap_or_default();
        agents.insert(
            "claude".into(),
            Arc::new(claude::build(&claude_cfg, Arc::clone(&sessions))),
        );

        let kimi_cfg = overrides.get("kimi").cloned().unwrap_or_default();
        agents.insert("kimi".into(), Arc::new(kimi::build(&kimi_cfg)));

        // Pooled Pi RPC adapter, configured from its own `[pi_rpc]` section.
        agents.insert("pi-rpc".into(), Arc::new(PiPoolAgent::new(&config.pi_rpc)));

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
        } else if agents.contains_key("codex") {
            // Codex is the first choice even when the configured default is
            // unknown: prefer it over the alphabetically first adapter.
            "codex".to_owned()
        } else {
            agents
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "codex".to_owned())
        };

        Self {
            agents,
            default,
            unavailable: Mutex::new(HashMap::new()),
        }
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

    /// All registered agent names in preference order: built-ins in
    /// [`BUILTIN_AGENTS`] order (so `codex` leads), then custom adapters
    /// alphabetically.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTIN_AGENTS
            .iter()
            .filter(|name| self.agents.contains_key(**name))
            .map(|name| (*name).to_owned())
            .collect();
        names.extend(
            self.agents
                .keys()
                .filter(|name| !BUILTIN_AGENTS.contains(&name.as_str()))
                .cloned(),
        );
        names
    }

    /// Mark an agent unavailable until `cooldown` has elapsed.
    pub fn mark_unavailable(&self, name: &str, cooldown: Duration) {
        let until = Instant::now() + cooldown;
        self.unavailable
            .lock()
            .expect("agent availability mutex poisoned")
            .insert(name.to_owned(), until);
        tracing::warn!(
            agent = %name,
            retry_after_secs = cooldown.as_secs(),
            "agent marked unavailable"
        );
    }

    /// Clear an agent's cooldown, making it eligible again immediately.
    pub fn mark_available(&self, name: &str) {
        self.unavailable
            .lock()
            .expect("agent availability mutex poisoned")
            .remove(name);
    }

    /// Whether an agent may currently be used. Expired entries are forgotten.
    pub fn is_available(&self, name: &str) -> bool {
        let now = Instant::now();
        let mut unavailable = self
            .unavailable
            .lock()
            .expect("agent availability mutex poisoned");
        match unavailable.get(name).copied() {
            Some(until) if until > now => false,
            Some(_) => {
                unavailable.remove(name);
                true
            }
            None => true,
        }
    }

    /// Names of the agents that are registered and not capacity-limited, in
    /// preference order (see [`Self::names`]); codex is first when available.
    pub fn available_names(&self) -> Vec<String> {
        self.names()
            .into_iter()
            .filter(|name| self.is_available(name))
            .collect()
    }
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRegistry")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .field("default", &self.default)
            .field("available", &self.available_names())
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

    #[test]
    fn codex_is_the_default_and_first_listed_choice() {
        let registry = AgentRegistry::from_config(&Config::default());
        assert_eq!(registry.default_name(), "codex");
        assert_eq!(registry.names().first().map(String::as_str), Some("codex"));
        assert_eq!(
            registry.available_names().first().map(String::as_str),
            Some("codex")
        );
    }

    #[test]
    fn unknown_default_falls_back_to_codex() {
        let config = Config {
            default_agent: "does-not-exist".into(),
            ..Default::default()
        };
        let registry = AgentRegistry::from_config(&config);
        assert_eq!(registry.default_name(), "codex");
    }

    #[test]
    fn capacity_limited_agents_are_skipped_until_the_cooldown_expires() {
        let registry = AgentRegistry::from_config(&Config::default());
        assert!(registry.is_available("codex"));
        assert!(registry.available_names().contains(&"codex".to_owned()));

        registry.mark_unavailable("codex", Duration::from_secs(60));
        assert!(!registry.is_available("codex"));
        assert!(!registry.available_names().contains(&"codex".to_owned()));
        // Other agents are unaffected.
        assert!(registry.available_names().contains(&"pi".to_owned()));

        // An already-expired entry is treated as available again.
        registry.mark_unavailable("codex", Duration::ZERO);
        assert!(registry.is_available("codex"));

        registry.mark_unavailable("codex", Duration::from_secs(60));
        registry.mark_available("codex");
        assert!(registry.is_available("codex"));
    }
}
