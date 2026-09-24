# Deploying forge-bot

forge-bot turns `@<bot>` mentions into agent runs. A mention can be discovered
two ways:

| Trigger | Needs |
| --- | --- |
| **Webhook** (preferred) | Repository owner/admin to register the hook |
| **Poller** (fallback) | Read access only |

Both feed the same pipeline (mention → policy → queue → agent → reply). The
supporting files live in [`contrib/`](contrib/):

| File | Purpose |
| --- | --- |
| `config.example.toml` (repo root) | Annotated configuration reference |
| [`doc/forgejo-webhook.md`](doc/forgejo-webhook.md) | Hook scopes, events and API examples |
| `contrib/forge-bot.service` | systemd unit |
| `contrib/forge-bot.env.example` | Environment/secret template |
| `contrib/install.sh` | Install binary + config + unit, enable the service |
| `contrib/run.sh` | Rootless launcher (`setsid` + `nohup`) |
| `contrib/detect-repo.sh` | Print `owner/repo` for the current git remote |
| `contrib/register-webhook.sh` | Create the Forgejo webhook via the API |
| `contrib/test-webhook.sh` | Send a signed test delivery |

**Secrets are never stored in this document or the config.** Supply the token
and webhook secret through the environment or the `0600` env file.

---

## 1. Build

```bash
cargo build --release
cargo test --all
```

## 2. Configure

```bash
cp config.example.toml forge-bot.toml
$EDITOR forge-bot.toml
```

The important fields:

* `mention`, `default_agent = "pi-rpc"`;
* `[forgejo]` `base_url`, `bot_username`;
* `[policy]` `allowed_users` / `allowed_repos`; an empty `allowed_repos` lets
  the configured users trigger the bot on any repository, while leaving both
  lists empty denies everyone;
* `[pi_rpc]` pool size, TTL, timeout, model/provider;
* `[poller]` `enabled = true` for the webhook-less fallback. With an empty
  `repositories` list the bot polls **every repository visible to its token**
  and refreshes that list every `discover_interval_secs`, so repositories
  created later are picked up automatically. Set `repositories = ["owner/repo"]`
  to restrict polling.

When a mention is found in a repository the bot cannot act on, it replies
`Permission Deny of <forge>` if it is still allowed to comment; if Forgejo also
denies commenting, the rejection is logged (nothing else can be delivered to
that thread).

Secrets come from the environment (or an `EnvironmentFile`):

```bash
export FORGEJO_TOKEN=...            # scopes: write:issue, write:repository
export FORGEJO_WEBHOOK_SECRET=...   # openssl rand -hex 32
```

Validate:

```bash
./target/release/forge-bot --config forge-bot.toml check
```

## 3. Register the webhook (owner/admin only)

```bash
export FORGEJO_URL=http://127.0.0.1:3000
export FORGEJO_TOKEN=...              # owner/admin token
export FORGEJO_WEBHOOK_SECRET=...
./contrib/register-webhook.sh
```

`REPO` defaults to the repository of the current git remote; set
`REPO=owner/repo` to target a different one. For organization, user (all of a
user's repositories) and system (whole instance) hooks, see
[`doc/forgejo-webhook.md`](doc/forgejo-webhook.md). Then set
`[poller] enabled = false`. If Forgejo refuses to deliver to loopback, add
`127.0.0.1` to `[webhook] ALLOWED_HOST_LIST` in `app.ini`, or keep the poller
enabled instead. A collaborator token cannot create hooks — use the poller in
that case.

## 4. Start

**systemd (root):**

```bash
sudo ./contrib/install.sh
journalctl -u forge-bot -f
```

**Rootless (no sudo):**

```bash
export FORGEJO_TOKEN=...
./contrib/run.sh ~/.config/forge-bot/forge-bot.toml
kill "$(cat ~/.local/state/forge-bot/forge-bot.pid)"   # stop
```

## 5. Verify

```bash
curl -s http://127.0.0.1:8080/healthz                  # ok
FORGEJO_WEBHOOK_SECRET=... ./contrib/test-webhook.sh   # signed delivery
```

Then post `@<bot> reply with pong` from an allowed user and check that a reply
comment appears. The first mention spawns a `pi --mode rpc` agent; later
mentions reuse it while it is idle.

## 6. Operations

* **Logs**: `journalctl -u forge-bot -f`, or `~/.local/state/forge-bot/forge-bot.log`.
* **State**: `state/jobs/`, `state/sessions/`, `state/poller.json`.
* **Upgrade**: rebuild, reinstall the binary, restart the service.
* **Uninstall**: stop and remove the unit/binary/config/state.

## 7. Troubleshooting

| Symptom | Fix |
| --- | --- |
| Hook creation says *"owner or admin write"* | Token is only a collaborator. Use an owner token or the poller. |
| Webhook `401` | Secret mismatch or missing `X-Forgejo-Signature`. |
| Forgejo cannot deliver to `127.0.0.1` | Allow loopback in `[webhook] ALLOWED_HOST_LIST`, or use the poller. |
| Poller never triggers | `poller.enabled = false`, or the token cannot see the repository. Reset `state/poller.json` if a cursor ran ahead. |
| `failed to spawn pi` | `pi` is not on the service account's `PATH`; set `[pi_rpc] command` to an absolute path. |
| No reply comment | `[reply] result = false`, or the token lacks `write:issue`. |
| Jobs pile up | Raise `[session] workers` / `[pi_rpc] max_agents`. |

## 8. This environment

* Bot runs as the `agent` account (which also owns the `pi` auth under
  `/home/agent/.pi`).
* Binary: `~/.local/bin/forge-bot`; config: `~/.config/forge-bot/forge-bot.toml`
  (mode `0600`); state: `~/.local/state/forge-bot/`.
* `shylock-bot` is only a collaborator, so the webhook cannot be created by the
  bot; `[poller] enabled = true` is used, with an empty `repositories` list so
  every repository visible to the bot is watched (new repositories included).
  Register the hook as `shylock` (or another admin) if you want push delivery,
  then disable the poller.
* A mention can only be answered on repositories where the bot can read the
  comments and post a reply; inaccessible repositories are skipped.
* Keep the token in the process environment only.
