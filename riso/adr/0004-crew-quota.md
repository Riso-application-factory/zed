+++
id = "ADR-0004"
title = "Crew quota core and Codex source"
status = "proposed"
review = "human"
intent = "Provide an app-wide attributed, freshness-aware quota API and a non-generative installed Codex source."
owns = ["crates/crew_quota/**"]
patches = ["Cargo.toml", "Cargo.lock", "crates/zed/Cargo.toml"]
hooks = ["crew-quota-init"]
watch = ["crates/zed/src/main.rs", "crates/project/src/agent_server_store.rs", "crates/project/src/agent_registry_store.rs", "crates/project/src/environment.rs", "crates/project/src/project_settings.rs", "crates/node_runtime/src/node_runtime.rs", "crates/db/src/kvp.rs", "crates/fs/src/fs.rs", "crates/gpui/src/app.rs", "crates/gpui/src/app/context.rs", "crates/workspace/src/workspace.rs"]
relies_on = ["ExternalAgentServer::install_status discovers installations without installing; get_command is intentionally never called", "Public registry and AllAgentServersSettings expose the configured npx package and environment overlays", "Registry npx installations live under external_agents_dir/registry/npx/<sanitized registry ID>/node_modules", "node_runtime::read_package_executable reads package.json bin without installation and selects its single entry or unscoped package name", "ProjectEnvironment::default_environment supplies the same base environment as the agent launcher", "NodeRuntime::binary_path is called only after Installed status; shared Node runtime bootstrap is accepted if missing, while agent installation and updates remain forbidden", "codex-acp >=2.5.0 and <3.0.0 stable versions support cli app-server passthrough", "Codex NDJSON initialize, initialized and account/rateLimits/read require no thread or turn", "KeyValueStore::global provides release-channel scoped storage", "Fs::watch supplies auth.json and config.toml change notifications without reading credential contents", "The quota init hook runs immediately after the model catalog init hook with AppState available"]
contracts = ["cargo test -p crew_quota", "ITERATIONS=20 cargo test -p crew_quota", "./script/clippy -p crew_quota", "cargo fmt --all -- --check", "cargo check -p zed"]
obsolete_if = "Upstream supplies an app-wide attributed quota service with equivalent freshness, persistence, scheduling and Codex account read contracts."
superseded_by = ""
+++

## Context

Quota account windows are distinct from thread context usage and estimated cost.
Crew consumers need immutable snapshots, explicit coverage and last-good data
whose freshness does not change on a failed read, reset countdown or restart.

## Decision

Own a pure domain core, reducer, validation and scheduler in `crew_quota`; keep
GPUI, inventory, process, registry and KVP types behind `upstream.rs` and its
provider module. Initialize once through `crew-quota-init`. This PR adds no UI,
HTTP observer, other provider adapters or credential mutations.

The Codex adapter resolves the existing registry installation without calling
`get_command`, which invokes npm installation even for an installed package.
Only the configured `codex-acp` registry npx package is supported; unknown launch
arguments, package layouts, prereleases and incompatible versions fail closed.
The adapter reads the installed version rather than trusting registry advertised
versions. Supported compatibility family is `>=2.5.0, <3.0.0`. Resolve bin using
the public package-executable reader and reject paths escaping the package.
Preserve project environment, npm Node-path additions, distribution environment
and settings overrides in producer order, without inheriting the worker identity.

Send only initialize, initialized and account/rateLimits/read. Bound startup to
15 seconds, reads to 25 seconds and output to 512 KiB; drain stderr without logging
its contents. Close stdin after the read, allow three seconds for exit, then kill
the owned process group; async-process reaps children, including cancellation.
No inference, thread/session, login, renewal or credit mutation RPC is sent.
No quota push is assumed. Prefer the map over its single-bucket fallback, retain
actual window durations and epoch-second resets, and keep credits un-denominated.
Unknown JSON-RPC errors are safe codes, not automatically authentication failures.

Schedule at two minutes with demand and fifteen minutes idle, with provider
floors, account single flight, two external operations, backoff and reset alarms.
Grok remains passive only, with no probes or probe budget. Claude and Antigravity
source implementations are deferred. A consumer asking for broader coverage than
the producer proves receives Partial, never a fabricated Fresh account.

Persist bounded schema-v1 sanitized records and attempt guards in `crew_quota`
scoped KVP through a serialized writer with five-second snapshot coalescing.
Attempt reservations are durable before launching any read and are restored
independently from snapshots; failure prevents launch. Future schemas are not
overwritten.
Since an installation cannot prove the same account after restart, preserve its
attempt guards but withhold numeric cache until a live read; decoder hydration
always marks persisted observations stale. Auth/config-file notifications invalidate the runtime generation, cancel shared
reads and remove numbers from the previous login. No credential identity or raw payload
is serialized, logged or returned to consumers.

## Consequences

The app-server may create home caches, databases and skills even without threads.
Crew never writes credentials or requests renewal, but producer startup renewal
behavior remains an integration risk; the spike verified only a fresh login.
A missing or unsupported producer stays explicit, without agent installation,
auto-login or fallback to another Codex account. After Installed status, the app
NodeRuntime may bootstrap the shared Node runtime if it is missing; this accepted
exception avoids duplicating upstream Node settings and path selection. The whole
launch resolver is bounded to fifteen seconds, and NotInstalled returns before
any call to NodeRuntime::binary_path.

This PR exposes the service API and Codex windows only. Later PRs add Claude,
Antigravity, the native xAI passive HTTP decorator, the Left/Right dock panel and
the status bar. Display defaults are Detailed/Remaining for the panel and
Compact/Remaining for the status bar, with used-fraction colors at 60/80.

## Merge notes

Reapply `crew-quota-init` after the exact model catalog init line. Absorb registry,
Node package/bin/environment, GPUI and KVP changes in the adapter; preserve the
no-install contract and the fake installation-tree test. Update this ADR before
adding an upstream patch, HTTP hook or provider surface.
