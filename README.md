# forge-bot

A forge-agnostic bot that turns `@agent` mentions into coding-agent runs.

Mention the bot in a comment on an issue or pull request:

```text
@agent investigate this test failure and fix it
```

and it will route the request to a coding agent (Codex, Pi, Claude Code, Kimi,
or anything you configure), which reads the surrounding context, changes code,
runs tests, pushes, and replies.

The design keeps a hard boundary:

```text
Forge → webhook → gateway → (location URL, message) → agent → does everything
```

The gateway never builds context for the agent. It hands it a location and a
message; the agent decides what to look at.

## How it works

```text
Forgejo / GitHub / GitLab
        │ webhook (signed)
        ▼
┌──────────────────────────────┐
│ Gateway                      │
│  verify signature            │
│  detect @agent mention       │
│  authorize author/repo       │
│  extract location + message  │
│  select agent                │
└──────────────┬───────────────┘
               │ AgentRequest { location, message }
               ▼
┌──────────────────────────────┐
│ Dispatcher                   │
│  bounded job queue           │
│  session persistence         │
│  workspace checkout          │
└──────────────┬───────────────┘
               ▼
   Codex · Pi · Claude Code · Kimi · custom
               │
               ├─ inspect forge
               ├─ inspect/modify code
               ├─ run commands / tests
               ├─ commit / push
               └─ reply
```

### Core contracts

The gateway only sends this:

```rust
pub struct AgentRequest {
    pub location: Url,
    pub message: String,
}
```

Forge and agent implementations are independent:

```rust
#[async_trait]
pub trait ForgeAdapter: Send + Sync {
    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()>;
    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<ForgeMessage>>;
    // ...
}

#[async_trait]
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, request: &AgentRequest, context: &AgentContext) -> Result<AgentOutcome>;
}
```

## Status

Implemented:

- [x] Forgejo webhook receiver
- [x] Webhook signature verification (HMAC-SHA256)
- [x] `@agent` mention detection (case-insensitive, optional `@agent:<name>`)
- [x] Authorization (allow-list users/repos, ignore self)
- [x] Location URL + message extraction
- [x] Codex adapter
- [x] Pi / Kimi / Claude Code adapters
- [x] Long-lived Pi RPC agent pool (`pi-rpc`): reuses an idle agent, spawns one when all are busy
- [x] Agent forge access (credentials via environment, optional checkout)
- [x] Bounded job queue + on-disk session/job persistence
- [x] GitHub and GitLab adapters
- [x] Per-user systemd service (no root)
- [x] Polling ingester for deployments where the bot cannot create a webhook: discovers every repository visible to the token and refreshes the list, so new repositories are picked up automatically

Still open (see the issue's roadmap):

- [ ] Stronger agent sandboxing / isolation
- [ ] Per-repository agent selection and richer per-thread sessions

## Quick start

```bash
cargo build --release

cp config.example.toml forge-bot.toml
$EDITOR forge-bot.toml

# Validate the configuration and see what was loaded.
cargo run -- check

# Run the server (default command).
cargo run -- serve

# Or run only the polling ingester, without binding a port.
cargo run -- poll
```

For a full deployment walkthrough — registering the Forgejo webhook, running
as a service, secrets, verification and troubleshooting — see
[`deploy.md`](deploy.md). The recommended way to run it is the per-user
systemd service, which needs no root and cannot touch other accounts:

```bash
cargo build --release
./contrib/install-user.sh      # systemctl --user status forge-bot
```

Secrets can be supplied through the environment instead of the file:

```bash
export FORGEJO_WEBHOOK_SECRET=...
export FORGEJO_TOKEN=...
cargo run -- serve
```

## Configuration

See [`config.example.toml`](config.example.toml) for the full reference. The
most important options:

| Key | Meaning |
| --- | --- |
| `bind` | Address the webhook server listens on. |
| `mention` | Trigger string, default `@agent`. |
| `default_agent` | Agent used when the mention does not pick one. |
| `[forgejo]` | `base_url`, `webhook_secret`, `token`, `bot_username`. |
| `[policy]` | `allow_all`, `allowed_users`, `allowed_repos`. |
| `[workspace]` | Whether to clone a checkout, and where. |
| `[reply]` | Whether the gateway posts ack/result comments. |
| `[session]` | Queue/state directory, worker count, recovery. |
| `[agents.<name>]` | Per-agent `command`, `args`, `prompt`, `timeout_secs`, `env`. |

Configuration is loaded from `FORGE_BOT_CONFIG` (or `--config`), falling back to
`forge-bot.toml`, falling back to defaults. Environment variables override file
values.

## Wiring a Forgejo webhook

See [`doc/forgejo-webhook.md`](doc/forgejo-webhook.md) for the full guide
(repository / organization / user / system scopes, events, API examples, the
loopback caveat, and verification).

Quick repository hook: **Settings → Webhooks → Add webhook → Forgejo**, target
`http://<host>:8080/webhooks/forgejo`, secret = `FORGEJO_WEBHOOK_SECRET`, event
**Issue comments**.

The endpoint also accepts GitHub (`/webhooks/github`) and GitLab
(`/webhooks/gitlab`) webhooks, selected by URL path.

## Trigger syntax

```text
@agent fix the failing test          # default agent
@agent:codex refactor this module    # pick an agent explicitly
@agent:pi review the diff            # any configured adapter
```

The mention is matched case-insensitively and only at a word boundary, so
`foo@agent.com` does not trigger it. Bot comments are ignored to avoid loops.

## Security

- Every webhook is signature-verified when a secret is configured. Never run
  without one in production.
- Authorization is separate from authentication. With no allow-list and no
  `allow_all`, the bot rejects everything.
- The bot's own login is added to the ignore list automatically.
- Agents receive a scoped forge token plus `FORGEJO_URL`/`FORGE_TOKEN` in their
  environment. They are *not* given an administrator token; grant only the
  scopes needed to comment and push.
- Workspaces are cloned with an authenticated URL and the credential is
  stripped from `origin` afterwards.
- `workspace.enabled = false` runs agents in an empty directory and lets them
  access the forge themselves.

## Project layout

```text
src/
├── agent/          # Agent trait + Codex/Pi/Claude/Kimi adapters
├── forge/          # ForgeAdapter trait + Forgejo/GitHub/GitLab
├── session/        # job queue, workers, session/job persistence
├── webhook.rs      # axum HTTP routes
├── config.rs       # configuration
├── location.rs     # URL → normalized ForgeLocation
├── mention.rs      # @agent parsing
├── policy.rs       # authorization
├── workspace.rs    # checkout management
├── forge_api.rs    # posting comments back
└── main.rs         # CLI entry point
```

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

The test suite covers URL parsing, mention extraction, HMAC verification,
payload normalization for all three forges, policy decisions, session
persistence, the dispatcher, and an end-to-end signed webhook flow.
