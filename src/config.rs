//! Configuration loading.
//!
//! Configuration is read from a TOML file (path given by `FORGE_BOT_CONFIG`,
//! defaulting to `forge-bot.toml`) and then selectively overridden by
//! environment variables so that secrets never have to live in the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{BotError, Result};

/// Fully resolved bot configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address the webhook server binds to.
    pub bind: String,
    /// Trigger string users mention, e.g. `@agent`.
    pub mention: String,
    /// Agent used when the mention does not select one.
    pub default_agent: String,
    /// Per-forge adapter configuration. Flattened so `[forgejo]`,
    /// `[github]` and `[gitlab]` are top-level tables.
    #[serde(flatten)]
    pub forges: Forges,
    pub policy: PolicyConfig,
    pub workspace: WorkspaceConfig,
    pub reply: ReplyConfig,
    pub session: SessionConfig,
    pub agents: AgentConfigs,
    /// Behaviour when an agent hits a quota, rate or capacity limit.
    /// `[quota]` is accepted as a backwards-compatible alias.
    #[serde(alias = "quota")]
    pub capacity: CapacityConfig,
    /// Long-lived Pi RPC agent pool (`pi-rpc` adapter).
    pub pi_rpc: PiRpcConfig,
    /// Forge polling ingester, used when webhooks cannot be configured.
    pub poller: PollerConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".to_owned(),
            mention: "@agent".to_owned(),
            default_agent: "codex".to_owned(),
            forges: Forges::default(),
            policy: PolicyConfig::default(),
            workspace: WorkspaceConfig::default(),
            reply: ReplyConfig::default(),
            session: SessionConfig::default(),
            agents: AgentConfigs::default(),
            capacity: CapacityConfig::default(),
            pi_rpc: PiRpcConfig::default(),
            poller: PollerConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from the given path (or the `FORGE_BOT_CONFIG`
    /// environment variable) and apply environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let env_path = std::env::var("FORGE_BOT_CONFIG").ok().map(PathBuf::from);
        let path = path.map(Path::to_path_buf).or(env_path);

        let mut config = match path {
            Some(path) => {
                if path.exists() {
                    let raw = std::fs::read_to_string(&path)?;
                    toml::from_str(&raw).map_err(|e| {
                        BotError::Config(format!("failed to parse {}: {e}", path.display()))
                    })?
                } else {
                    Config::default()
                }
            }
            None => Config::default(),
        };

        config.apply_defaults();
        config.apply_env();
        Ok(config)
    }

    fn apply_defaults(&mut self) {
        if self.bind.is_empty() {
            self.bind = "0.0.0.0:8080".to_owned();
        }
        if self.mention.is_empty() {
            self.mention = "@agent".to_owned();
        }
        if self.default_agent.is_empty() {
            self.default_agent = "codex".to_owned();
        }
        self.policy.ensure_defaults();
        self.workspace.ensure_defaults();
        self.session.ensure_defaults();
        self.capacity.ensure_defaults();
        if self.pi_rpc.command.is_empty() {
            self.pi_rpc.command = "pi".to_owned();
        }
        if self.pi_rpc.max_agents == 0 {
            self.pi_rpc.max_agents = 1;
        }
        if self.pi_rpc.timeout_secs == 0 {
            self.pi_rpc.timeout_secs = 1800;
        }
        if self.poller.interval_secs == 0 {
            self.poller.interval_secs = 15;
        }
        if self.poller.discover_interval_secs == 0 {
            self.poller.discover_interval_secs = 300;
        }
        if self.poller.page_limit == 0 {
            self.poller.page_limit = 50;
        }
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("FORGE_BOT_BIND") {
            self.bind = v;
        }
        if let Ok(v) = std::env::var("FORGE_BOT_MENTION") {
            self.mention = v;
        }
        if let Ok(v) = std::env::var("FORGE_BOT_DEFAULT_AGENT") {
            self.default_agent = v;
        }
        if let Ok(v) = std::env::var("FORGE_BOT_MAX_CONCURRENCY")
            && let Ok(n) = v.parse()
        {
            self.session.workers = n;
        }
        if let Ok(v) = std::env::var("FORGE_BOT_WORKSPACE_ROOT") {
            self.workspace.root = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("FORGE_BOT_SESSION_DIR") {
            self.session.dir = PathBuf::from(v);
        }

        if let Some(forgejo) = self.forges.forgejo.as_mut() {
            if let Ok(v) = std::env::var("FORGEJO_WEBHOOK_SECRET") {
                forgejo.webhook_secret = Some(v);
            }
            if let Ok(v) = std::env::var("FORGEJO_TOKEN") {
                forgejo.token = Some(v);
            }
            if let Ok(v) = std::env::var("FORGEJO_BASE_URL") {
                forgejo.base_url = v;
            }
            if let Ok(v) = std::env::var("FORGEJO_BOT_USERNAME") {
                forgejo.bot_username = Some(v);
            }
        }
    }

    /// Resolve the trigger string to its canonical form.
    pub fn trigger(&self) -> &str {
        self.mention.trim()
    }

    /// Credentials exposed to agents for the given forge. A generic
    /// `FORGE_URL` / `FORGE_TOKEN` pair is always included so agents can be
    /// forge agnostic.
    pub fn credentials_for(&self, forge: crate::location::ForgeKind) -> Vec<(String, String)> {
        use crate::location::ForgeKind;

        let mut creds: Vec<(String, String)> = Vec::new();
        match forge {
            ForgeKind::Forgejo | ForgeKind::Gitea => {
                if let Some(cfg) = &self.forges.forgejo {
                    creds.push(("FORGEJO_URL".into(), cfg.base_url.clone()));
                    if let Some(token) = &cfg.token {
                        creds.push(("FORGEJO_TOKEN".into(), token.clone()));
                        creds.push(("GITEA_TOKEN".into(), token.clone()));
                    }
                }
            }
            ForgeKind::GitHub => {
                if let Some(cfg) = &self.forges.github {
                    creds.push(("GITHUB_URL".into(), cfg.base_url.clone()));
                    if let Some(token) = &cfg.token {
                        creds.push(("GITHUB_TOKEN".into(), token.clone()));
                        creds.push(("GH_TOKEN".into(), token.clone()));
                    }
                }
            }
            ForgeKind::GitLab => {
                if let Some(cfg) = &self.forges.gitlab {
                    creds.push(("GITLAB_URL".into(), cfg.base_url.clone()));
                    if let Some(token) = &cfg.token {
                        creds.push(("GITLAB_TOKEN".into(), token.clone()));
                    }
                }
            }
            ForgeKind::Unknown => {}
        }

        if let Some((url, token)) = self.generic_credentials(forge) {
            creds.push(("FORGE_URL".into(), url));
            creds.push(("FORGE_TOKEN".into(), token));
        }

        creds
    }

    fn generic_credentials(&self, forge: crate::location::ForgeKind) -> Option<(String, String)> {
        use crate::location::ForgeKind;
        match forge {
            ForgeKind::Forgejo | ForgeKind::Gitea => self
                .forges
                .forgejo
                .as_ref()
                .and_then(|c| c.token.clone().map(|t| (c.base_url.clone(), t))),
            ForgeKind::GitHub => self
                .forges
                .github
                .as_ref()
                .and_then(|c| c.token.clone().map(|t| (c.base_url.clone(), t))),
            ForgeKind::GitLab => self
                .forges
                .gitlab
                .as_ref()
                .and_then(|c| c.token.clone().map(|t| (c.base_url.clone(), t))),
            ForgeKind::Unknown => None,
        }
    }
}

/// Per-forge adapter configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Forges {
    pub forgejo: Option<ForgejoConfig>,
    pub github: Option<GithubConfig>,
    pub gitlab: Option<GitlabConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ForgejoConfig {
    /// Base URL of the Forgejo instance, e.g. `https://forge.example.com`.
    pub base_url: String,
    /// Shared secret configured on the webhook.
    pub webhook_secret: Option<String>,
    /// Token used by the bot to post comments / clone repositories.
    pub token: Option<String>,
    /// Username of the bot, used to ignore its own comments.
    pub bot_username: Option<String>,
}

impl Default for ForgejoConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:3000".to_owned(),
            webhook_secret: None,
            token: None,
            bot_username: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubConfig {
    pub base_url: String,
    pub webhook_secret: Option<String>,
    pub token: Option<String>,
    pub bot_username: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GitlabConfig {
    pub base_url: String,
    pub webhook_secret: Option<String>,
    pub token: Option<String>,
    pub bot_username: Option<String>,
}

/// Who is allowed to trigger the bot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct PolicyConfig {
    /// When true, every user and repository is allowed. Intended for local
    /// debugging only.
    pub allow_all: bool,
    pub allowed_users: Vec<String>,
    pub allowed_repos: Vec<String>,
    /// Users whose comments are always ignored (normally the bot itself).
    pub ignored_users: Vec<String>,
}

impl PolicyConfig {
    fn ensure_defaults(&mut self) {
        if self.allowed_users.is_empty() && self.allowed_repos.is_empty() {
            // An empty policy would silently deny everything. Be explicit about
            // the safe default while still allowing operators to opt into
            // `allow_all`.
            if !self.allow_all {
                tracing::warn!(
                    "policy: no allowed_users/allowed_repos configured; the bot will reject \
                     every trigger until the policy is filled in or `allow_all = true` is set"
                );
            }
        }
    }
}

/// Where agents get a checkout of the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Clone the repository before invoking an agent.
    pub enabled: bool,
    pub root: PathBuf,
    /// Reuse a workspace for the same repository number across turns.
    pub reuse: bool,
    /// Git author used for agent commits.
    pub git_author_name: String,
    pub git_author_email: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: PathBuf::from("/tmp/forge-bot-workspaces"),
            reuse: true,
            git_author_name: "forge-bot".to_owned(),
            git_author_email: "forge-bot@localhost".to_owned(),
        }
    }
}

impl WorkspaceConfig {
    fn ensure_defaults(&mut self) {
        if self.root.as_os_str().is_empty() {
            self.root = PathBuf::from("/tmp/forge-bot-workspaces");
        }
        if self.git_author_name.is_empty() {
            self.git_author_name = "forge-bot".to_owned();
        }
        if self.git_author_email.is_empty() {
            self.git_author_email = "forge-bot@localhost".to_owned();
        }
    }
}

/// Whether the gateway should post a comment back to the forge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplyConfig {
    /// Post a short acknowledgement when a job starts.
    pub ack: bool,
    /// Post a result comment when a job finishes. Agents may reply themselves;
    /// set to false to avoid duplicate comments.
    pub result: bool,
}

impl Default for ReplyConfig {
    fn default() -> Self {
        Self {
            ack: true,
            result: true,
        }
    }
}

/// Job queue and session persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Directory used to persist jobs and sessions.
    pub dir: PathBuf,
    /// Maximum number of concurrently running agent jobs.
    pub workers: usize,
    /// Maximum size of the in-memory job queue.
    pub queue_capacity: usize,
    /// Requeue persisted (unfinished) jobs on startup.
    pub recover: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("/tmp/forge-bot-state"),
            workers: 2,
            queue_capacity: 256,
            recover: true,
        }
    }
}

impl SessionConfig {
    fn ensure_defaults(&mut self) {
        if self.dir.as_os_str().is_empty() {
            self.dir = PathBuf::from("/tmp/forge-bot-state");
        }
        if self.workers == 0 {
            self.workers = 1;
        }
        if self.queue_capacity == 0 {
            self.queue_capacity = 256;
        }
    }
}

/// Configuration for each agent adapter, keyed by adapter name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfigs {
    pub default: String,
    /// Per-agent overrides. Flattened so `[agents.codex]` works in TOML.
    #[serde(flatten)]
    pub overrides: BTreeMap<String, AgentConfig>,
}

impl AgentConfigs {
    pub fn get(&self, name: &str) -> Option<&AgentConfig> {
        self.overrides.get(name)
    }
}

/// How to launch one agent adapter.
///
/// All fields are optional so that a partial override (for example only the
/// command) keeps the adapter's sensible defaults for the rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub command: Option<String>,
    /// Extra arguments passed before the prompt.
    pub args: Option<Vec<String>>,
    /// How the prompt is delivered: `stdin` (default) or `arg`.
    pub prompt: Option<PromptDelivery>,
    /// Maximum runtime in seconds.
    pub timeout_secs: Option<u64>,
    /// Extra environment variables for the process.
    pub env: BTreeMap<String, String>,
    /// Whether the agent runs with an unrestricted sandbox. Recorded for
    /// operators and forwarded to adapters that understand it.
    pub dangerously_skip_permissions: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum PromptDelivery {
    #[default]
    Stdin,
    Arg,
}

/// How the dispatcher reacts when an agent hits a capacity limit.
///
/// A failed run whose output looks like a quota, rate-limit or
/// capacity/overload message (see [`crate::agent::capacity`]) marks that agent
/// as unavailable for `cooldown_secs` and, when `fallback` is enabled, retries
/// the job with the next available agent. If no agent is available the bot
/// replies `No available agent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CapacityConfig {
    /// Retry a job on another available agent when the chosen one is at
    /// capacity. When false the job simply fails and the agent is still
    /// skipped for `cooldown_secs`.
    pub fallback: bool,
    /// How long (seconds) an agent is skipped after it reports a limit.
    pub cooldown_secs: u64,
    /// Extra, case-insensitive substrings that count as a capacity message, in
    /// addition to the built-in markers.
    pub markers: Vec<String>,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            fallback: true,
            cooldown_secs: 3600,
            markers: Vec::new(),
        }
    }
}

impl CapacityConfig {
    fn ensure_defaults(&mut self) {
        if self.cooldown_secs == 0 {
            self.cooldown_secs = 3600;
        }
    }
}

/// Configuration for the long-lived Pi RPC agent pool.
///
/// Unlike the one-shot `pi` adapter, `pi-rpc` keeps `pi --mode rpc`
/// subprocesses alive and reuses them for subsequent requests. When every
/// agent in the pool is busy (or the pool is empty) a new one is spawned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PiRpcConfig {
    /// The `pi` executable to run.
    pub command: String,
    /// Extra arguments appended after `--mode rpc` (and the flags below).
    pub args: Vec<String>,
    /// Maximum number of live `pi` processes.
    pub max_agents: usize,
    /// Kill an unused agent after this many seconds.
    pub idle_ttl_secs: u64,
    /// Maximum runtime of a single request.
    pub timeout_secs: u64,
    /// Pass `--approve` so project-local files are trusted.
    pub approve: bool,
    /// Disable pi's session persistence (`--no-session`).
    pub no_session: bool,
    /// Optional model override, e.g. `deepseek-flash`.
    pub model: Option<String>,
    /// Optional provider override, e.g. `deepseek`.
    pub provider: Option<String>,
    /// Extra environment variables for the spawned agents.
    pub env: BTreeMap<String, String>,
}

impl Default for PiRpcConfig {
    fn default() -> Self {
        Self {
            command: "pi".to_owned(),
            args: Vec::new(),
            max_agents: 2,
            idle_ttl_secs: 900,
            timeout_secs: 1800,
            approve: true,
            no_session: true,
            model: None,
            provider: None,
            env: BTreeMap::new(),
        }
    }
}

/// Configuration for the polling ingester.
///
/// Forge webhooks are the preferred trigger, but a bot account that is only a
/// repository collaborator cannot create them. Polling reuses the same
/// pipeline with nothing more than a read token.
///
/// When `repositories` is empty the poller discovers every repository visible
/// to the token (via `/repos/search`) and refreshes that list every
/// `discover_interval_secs`, so newly created repositories are picked up
/// automatically. Set `repositories` to an explicit `owner/repo` list to
/// restrict polling instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PollerConfig {
    pub enabled: bool,
    /// Seconds between polls.
    pub interval_secs: u64,
    /// Explicit repositories to watch; empty means "all visible repositories".
    pub repositories: Vec<String>,
    /// How often to refresh the discovered repository list.
    pub discover_interval_secs: u64,
    /// How far back to look for a repository the first time it is seen.
    pub lookback_secs: u64,
    /// Page size for the comments and repository search endpoints.
    pub page_limit: usize,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 15,
            repositories: Vec::new(),
            discover_interval_secs: 300,
            lookback_secs: 3600,
            page_limit: 50,
        }
    }
}

/// Expand a leading `~` in a path into `$HOME`.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    } else if s == "~"
        && let Some(home) = dirs_home()
    {
        return home;
    }
    path.to_path_buf()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = Config::default();
        let mut config = config;
        config.apply_defaults();
        assert_eq!(config.bind, "0.0.0.0:8080");
        assert_eq!(config.mention, "@agent");
        assert_eq!(config.default_agent, "codex");
        assert_eq!(config.session.workers, 2);
        assert!(config.workspace.enabled);
    }

    #[test]
    fn parses_toml() {
        let raw = r#"
bind = "127.0.0.1:9000"
mention = "@bot"
default_agent = "pi"

[forgejo]
base_url = "https://forge.example.com"
webhook_secret = "s3cret"
token = "tok"
bot_username = "botty"

[policy]
allowed_users = ["alice"]

[capacity]
fallback = false
cooldown_secs = 120
markers = ["no tokens left"]

[agents.codex]
command = "codex"
args = ["exec", "-"]
timeout_secs = 60
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.bind, "127.0.0.1:9000");
        assert_eq!(config.mention, "@bot");
        let forgejo = config.forges.forgejo.unwrap();
        assert_eq!(forgejo.base_url, "https://forge.example.com");
        assert_eq!(config.agents.get("codex").unwrap().timeout_secs, Some(60));
        assert!(!config.capacity.fallback);
        assert_eq!(config.capacity.cooldown_secs, 120);
        assert_eq!(config.capacity.markers, vec!["no tokens left".to_owned()]);
    }

    #[test]
    fn accepts_quota_as_an_alias_for_capacity() {
        let raw = r#"
[quota]
fallback = true
cooldown_secs = 42
markers = ["overloaded"]
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert!(config.capacity.fallback);
        assert_eq!(config.capacity.cooldown_secs, 42);
        assert_eq!(config.capacity.markers, vec!["overloaded".to_owned()]);
    }

    #[test]
    fn expands_tilde() {
        assert_eq!(expand_tilde(Path::new("/abs/x")), PathBuf::from("/abs/x"));
        // `~/x` either expands against $HOME or is left untouched; in both
        // cases the final component is `x`.
        assert!(expand_tilde(Path::new("~/x")).ends_with("x"));
    }
}
