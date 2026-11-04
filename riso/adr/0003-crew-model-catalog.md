+++
id = "ADR-0003"
title = "Crew external agent model catalog"
status = "proposed"
review = "human"
intent = "Keep a fresh, persisted catalog of the models every installed and logged-in local External Agent publishes, discovered actively, as a foundation for other Crew features."
owns = ["crates/crew_model_catalog/**"]
patches = ["Cargo.toml", "Cargo.lock", "crates/zed/Cargo.toml", "crates/project/src/agent_server_store.rs"]
hooks = ["crew-model-catalog-init"]
watch = ["crates/agent_servers/src/acp/transport.rs", "crates/acp_thread/src/acp_thread.rs", "crates/acp_thread/src/connection.rs", "crates/acp_thread/src/config_options.rs", "crates/agent_servers/src/agent_servers.rs", "crates/agent_servers/src/custom.rs", "crates/agent_servers/src/acp/**", "crates/project/src/agent_server_store.rs", "crates/project/src/project.rs", "crates/db/src/kvp.rs", "crates/gpui/src/app.rs", "crates/workspace/src/workspace.rs"]
relies_on = ["ExternalAgentServer::install_status is read-only and callable after connect to report the version get_command installed", "External agents publish models only as ACP config options of category Model; only the native agent implements model_selector", "CustomAgentServer::connect with an own AgentServerDelegate and Project works without agent_ui", "AgentConnection::new_session fails with an error AuthRequired::from_error recognizes when the agent is not logged in", "AgentConnection::agent_version reports the running agent version", "AcpThreadEvent::ConfigOptionsUpdated carries the full option list", "observe_new observers run on effect flush, while the new entity is still being updated", "Project::local accepts LocalProjectFlags to disable worktree trust and global config watching", "KeyValueStore::global is the release-channel app database with scoped namespaces", "AppState is available where settings_ui::init(cx) runs in main.rs"]
contracts = ["cargo test -p crew_model_catalog", "cargo test -p project --features test-support --lib install_status", "cargo check -p zed"]
obsolete_if = "Upstream maintains a fresh, app-wide catalog of the models external agents publish."
superseded_by = ""
+++

## Context

Crew features such as orchestration must know which models each External Agent
offers without the Human opening every agent, and must not decide on stale data.
ACP only reveals an agent's models through the config options of a session.

## Decision

Own `crates/crew_model_catalog`, split into a core (`crew_model_catalog.rs`,
`extract.rs`) that knows no upstream types and an adapter (`upstream.rs`) that is
the only module touching `acp_thread`, `agent_servers`, `project`, `db` and
`workspace`. A test in the crate fails if the core references those crates.

The adapter:

- lists agents through the `AgentServerStore` of a hidden local project;
- probes each installed agent on its own connection with a throwaway session in
  `data_dir/crew/model_catalog/probe`, then closes the session and shuts the
  connection down;
- reads the models of the Human's local threads through `observe_new::<AcpThread>`;
- persists one JSON record per agent in the scoped KVP namespace
  `crew_model_catalog`.

The core schedules probes:

- at startup and every five minutes;
- on agent server changes;
- on demand through `ensure_fresh`.

Probes run at most two at a time, share one run per agent, and back off
exponentially after failures.

Every answer carries freshness:

- **Fresh** up to 6 hours.
- **Stale** up to 24 hours.
- **Expired** after that, or immediately when the installed agent version
  changes or the agent is not logged in.

`check_model` only reports `Available` from fresh data.

The one upstream patch adds `ExternalAgentServer::install_status` with a default,
implemented for registry archive and npx agents. It reports the installation
without downloading or creating directories. Without it, callers could only learn
about an installation through `get_command`, which installs. The patch adds
methods and helpers and leaves existing code paths unchanged.

## Consequences

Probing spawns the agent process. It also starts the MCP servers configured for
the hidden project, including the built-in Jev entry. An npx agent may update
itself when the registry has a newer version. A probe session may remain in the
agent's own history, under the probe directory. Agents that are not logged in
are reported as such, without UI or notifications. There is no user setting to
disable probing.

Known risk: upstream `crates/agent_servers/src/acp/transport.rs` logs raw agent
stderr at warn level. Background probes can trigger these logs, including any
sensitive content an agent writes to stderr; the catalog does not control that
upstream logging path.

## Merge notes

Reapply the `crew-model-catalog-init` hook after `settings_ui::init(cx);`.

When `agent_server_store.rs` conflicts:

- keep `AgentInstallStatus` and the trait default;
- keep `registry_platform_key` in step with the platform key `get_command` builds;
- keep `npx_install_dir` in step with the directory `get_command` installs into.

Upstream changes to ACP config option types, `AgentConnection` or
`AgentServer::connect` are absorbed in `crates/crew_model_catalog/src/upstream.rs`.
