+++
id = "ADR-0004"
title = "Crew quota service, provider sources and dock panel"
status = "proposed"
review = "human"
intent = "Provide an app-wide attributed, freshness-aware quota API with installed Codex reads, Claude passive updates and demand-driven probes, Antigravity usage reads, and passive native xAI headers."
owns = ["crates/crew_quota/**"]
patches = ["Cargo.toml", "Cargo.lock", "crates/zed/Cargo.toml"]
hooks = ["crew-quota-init", "crew-quota-http"]
watch = ["crates/agent_ui/src/agent_panel.rs", "crates/agent_ui/src/conversation_view.rs", "crates/agent/src/thread.rs", "crates/language_models/src/provider/x_ai.rs", "crates/acp_thread/src/acp_thread.rs", "crates/acp_thread/src/connection.rs", "crates/agent_servers/src/acp/v1.rs", "crates/agent_servers/src/acp/v2.rs", "crates/agent_servers/src/acp/debug_log.rs", "crates/http_client/src/http_client.rs", "crates/zed/src/main.rs", "crates/project/src/agent_server_store.rs", "crates/project/src/agent_registry_store.rs", "crates/project/src/environment.rs", "crates/project/src/project_settings.rs", "crates/node_runtime/src/node_runtime.rs", "crates/db/src/kvp.rs", "crates/fs/src/fs.rs", "crates/gpui/src/app.rs", "crates/gpui/src/app/context.rs", "crates/workspace/src/workspace.rs", "crates/workspace/src/dock.rs", "crates/ui/src/components/button/button_like.rs", "crates/ui/src/components/button/icon_button.rs", "crates/gpui/src/elements/div.rs"]
relies_on = ["AgentPanel active_conversation_view matches visible_surface base_view variants; is_visible gates dock visibility, and ConversationView agent_key identifies the current thread agent", "The early crew-quota-http hook decorates the app HttpClient before Client and native providers are constructed; send, proxy and user_agent retain upstream semantics", "Native xAI authentication presence and effective API URL define a nonsecret scope; keys and request headers are never read", "Antigravity binary resolution requires Installed current registry version and exactly one v_<sanitized-version>_ directory containing the target command; it never calls get_command", "Antigravity environment parity follows LocalRegistryArchiveAgent get_command: ProjectEnvironment default, registry platform target env, then settings env, with target args and darwin/linux platform keys", "Antigravity ACP 1.4.5 usage is a local Markdown command; session/close is verified despite missing capability advertisement; only advertised session/delete is sent", "Antigravity cleanup uses an exact-ID durable journal, directory identity and no-follow directory-relative unlinks only after owned Unix group exit; unknown writers or ownership suspend polling", "AcpThread exposes local project, agent connection and session ID; defer avoids entity reentry", "Both concrete ACP connections expose subscribe_debug_messages with timestamp-free backlog and a live receiver", "Default registry claude-acp without env, args or config overrides shares the ProjectEnvironment shell-OAuth login with the probe; overridden or custom connections remain isolated", "Scope keys hash only env key names and nonsecret configuration flags, never secret values; residual risk: project-local environment or login changes may differ from the hidden project without an explicit upstream auth identity", "Claude ACP rateLimit utilization is a consumed fraction and resetsAt is Unix seconds; unifiedWindows takes precedence over representative fields", "Claude tiny Haiku messages probe uses fixed OAuth headers and max_tokens 1; stored credentials are presence-detected only and unsupported", "ExternalAgentServer::install_status discovers installations without installing; get_command is intentionally never called", "Public registry and AllAgentServersSettings expose the configured npx package and environment overlays", "Registry npx installations live under external_agents_dir/registry/npx/<sanitized registry ID>/node_modules", "node_runtime::read_package_executable reads package.json bin without installation and selects its single entry or unscoped package name", "ProjectEnvironment::default_environment supplies the same base environment as the agent launcher", "NodeRuntime::binary_path is called only after Installed status; shared Node runtime bootstrap is accepted if missing, while agent installation and updates remain forbidden", "codex-acp >=2.5.0 and <3.0.0 stable versions support cli app-server passthrough", "Codex NDJSON initialize, initialized and account/rateLimits/read require no thread or turn", "Unix process groups own the Codex launcher tree; Windows and other unsupported platforms reject before spawn until atomic process-tree ownership exists", "KeyValueStore::global provides release-channel scoped storage", "Fs::watch supplies auth.json and config.toml change notifications without reading credential contents", "The quota init hook runs immediately after the model catalog init hook with AppState available", "Workspace observe_new receives its creation window before asynchronous dock restoration completes", "Workspace add_panel restores width through the stable panel_key", "Dock panel activation priorities must be unique; Crew quota reserves 100", "Dock relocation observes SettingsStore changes; Crew defers a SettingsStore pulse after changing its own KVP preference"]
contracts = ["cargo test -p crew_quota upstream::grok", "cargo test -p crew_quota upstream::antigravity", "cargo test -p crew_quota upstream::claude", "cargo test -p crew_quota", "ITERATIONS=20 cargo test -p crew_quota", "./script/clippy -p crew_quota", "cargo fmt --all -- --check", "cargo check -p zed"]
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
provider and panel modules. Initialize once through `crew-quota-init`. Install crew-quota-http immediately after the app HttpClient is set, before Client construction.

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
the owned Unix process group and await the direct child's exit; async-process
also reaps cancellation paths. Windows reads fail closed as UnsupportedPlatform
before spawning: the current std/async-process API cannot assign a kill-on-close
Job Object atomically before the launcher executes. A post-spawn assignment
would leave a descendant escape race, so Windows remains unsupported until an
adapter can establish ownership at process creation. Synthetic subprocess tests
exercise success, malformed protocol, timeout and cancellation on Unix; the same
tests assert rejection without a launcher side effect on Windows.
No inference, thread/session, login, renewal or credit mutation RPC is sent.
No quota push is assumed. Prefer the map over its single-bucket fallback, retain
actual window durations and epoch-second resets, and keep credits un-denominated.
Unknown JSON-RPC errors are safe codes, not automatically authentication failures.

Schedule at two minutes with demand and fifteen minutes idle, with provider
floors, account single flight, two external operations, backoff and reset alarms.
Grok remains passive only, with no probes or probe budget. Antigravity uses a dedicated bounded ACP connection, one session and one advertised `/usage` prompt per attempt, with a fifteen-minute attempt floor and protocol/cleanup latches. A consumer asking for broader coverage than
the producer proves receives Partial, never a fabricated Fresh account.

Persist bounded schema-v1 sanitized records and attempt guards in `crew_quota`
scoped KVP through a serialized writer with five-second snapshot coalescing.
Attempt reservations are durable before launching any read and are restored
independently from snapshots; failure prevents launch. Attempts more than five
minutes ahead of UTC and restored retry times beyond the supported one-hour
backoff are rejected with a persistence diagnostic. Accepted restored guards and
runtime cadence/backoff use monotonic deadlines, so clock rollback cannot renew
a guard indefinitely. Future schemas are not overwritten.

Freshness checks cover the consumer's requested fraction/remaining/limit fields;
by default all known numeric fields must be fresh. Retention expires fields
independently, preserving a fresh amount when an old fraction expires. Passive
availability transitions and invalidation removals emit their typed events.
Since an installation cannot prove the same account after restart, preserve its
attempt guards but withhold numeric cache until a live read; decoder hydration
always marks persisted observations stale. Auth/config-file notifications invalidate the runtime generation, cancel shared
reads and remove numbers from the previous login. No credential identity or raw payload
is serialized, logged or returned to consumers.

Antigravity supports the inspected ACP 1.4.5 Markdown grammar, including group
descriptions, rounded percentages, integer units, Disabled rows and session tier.
Unknown structural rows and activity requests fail closed. Reads require both
headings, end_turn and a recognized numeric bucket; no quota freshness is inferred
from session context usage. The adapter retains only recognized group/window
labels and numeric values. Reset instants use UTC minute precision; malformed
reset text leaves the instant unknown. Raw transcripts are never persisted.

The adapter owns its minimal NDJSON transport rather than using CustomAgentServer:
the shared connection shutdown API cannot prove descendant exit before SQLite
unlinking. It reuses quota's Unix process ownership primitive without changing
shared ACP lifecycle APIs. Normal configured MCP startup is accepted, and neither
GEMINI_HOME nor settings are rewritten. Startup can refresh the producer's own
credentials internally; no verified-zero-inference claim is made.

A private durable journal holds only the exact owned session, process group,
directory identity and prelaunch filenames. Recovery checks group absence before
unlinking; a live/reused group or an unknown session remains pending for Human
intervention. Cancellation leaves bounded cleanup running, serialized across
Antigravity reads. Protocol and cleanup failures suspend automatic polling.

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

The dock panel attaches synchronously through `observe_new::<Workspace>` without
waiting for provider data. It owns a stable `CrewQuotaPanel` persistent name and
`crew_quota` size key, uses generic workspace width persistence, and exposes the
workspace dock button plus `crew_quota::ToggleFocus` and `RefreshQuota` actions
without default keybindings. Crew-scoped KVP stores Left/Right placement and
Detailed/Compact and Used/Remaining preferences. Preferences load before
workspace creation; writes are serialized. Dock relocation requires a deferred
SettingsStore notification because the upstream dock does not observe a panel
position event. Panel activation holds a renewable service demand lease; one
local minute timer updates countdowns without provider calls. Unsupported provider
sources remain explicit and cached data retains stale/error labels. A workspace
created without a window attaches on its first window render through a deferred
action renderer; the usual creation-window path remains synchronous.

The workspace observer attaches quota windows for the visible thread agent only.
The existing dock Quota button is the sole panel toggle; the status item adds no
second button. Public AgentPanel::is_visible
and active_conversation_view gate display; ConversationView::agent_key supplies
the agent ID; native xAI threads use as_native_thread and Thread::model provider_id. In this checkout active_conversation_view (agent_panel.rs:4041)
and visible_surface (:4418) match the same base_view variants, so no upstream
patch is needed. Observe dock visibility, AgentPanel and ConversationView changes
without acquiring demand or increasing the idle cadence. Detailed/Compact and
Remaining/Used remain shared display preferences.


## Merge notes

Reapply `crew-quota-init` after the exact model catalog init line. Absorb registry,
Node package/bin/environment, GPUI and KVP changes in the adapter; preserve the
no-install contract and the fake installation-tree test. Update this ADR before
adding an upstream patch, HTTP hook or provider surface.

## Debug fixture preview

Debug builds expose `crew_quota::ToggleFixturePreview` and opt-in
`CREW_QUOTA_PREVIEW=overview|states` for sanitized, panel-local fixture snapshots.
Optional `CREW_QUOTA_PREVIEW_COMPACT=1`, `CREW_QUOTA_PREVIEW_USED=1` and
`CREW_QUOTA_PREVIEW_LEFT=1` select a review layout without persisting it.
The sample-data banner identifies the preview; the scene button switches between
provider window examples and an availability-state roster. Refresh reloads only
the fixture, display changes remain local, and preview activation acquires no
service demand. Neither snapshots nor fixture annotations enter the quota
service or KVP. The fixture module and action are excluded from release builds.

Fixtures show Codex 5h/Week, Claude 5h/Week, two Antigravity groups each with
5h/Week, and the latest Grok request/token amounts without resets. The states
scene includes a signed-out Claude; missing or not-installed agents are hidden.
Each agent has one account row. Detailed shows bars, percentages and available
reset countdowns; Compact places the window values on one agent line. Credits,
overage, descriptions, per-model Grok labels and duplicate badges are omitted.
Core metadata remains compatible for existing acquisition and consumer contracts.

## Provider disclosure and compact header

Crew KVP `panel_preferences.collapsed` records a set of provider IDs; a missing
field or absent provider defaults to expanded. Disclosure state is shared across
Detailed/Compact and workspace views. The debug fixture keeps this preference
local like its other display controls; `CREW_QUOTA_PREVIEW_COLLAPSED=mixed`
starts with Grok and Antigravity collapsed for review. Provider headers retain
the most constrained known percentage and severity and expose keyboard
activation plus expanded state. Their focus handles are stable per provider.

The existing `SignalHigh` icon provides four rising usage bars for the dock
button. No upstream icon list or assets change. Detailed/Compact and
Remaining/Used use paired, labeled icon toggles beside Refresh on one header
row, including at the panel minimum width of 280 px.

## Claude source

Observe local Claude threads after their entity update, downcast v1/v2 concrete
connections and drain one public debug feed per connection. Exact known session
IDs and incoming usage_update envelopes gate a bounded field allowlist; context
usage, model labels, unrelated traffic and unknown fields are discarded. Backlog
uses persisted provenance and cannot freshen windows. Live events and Haiku
headers share one fraction/epoch normalizer, with independent 5h, 7d and overage
status; overage rejection does not exhaust the base allowance.

Default configured registry claude-acp without environment, argument or config
overrides shares the shell-OAuth probe scope. Custom/overridden/unknown origins
receive isolated connection scopes, keyed by nonsecret configuration flags and
environment key names, never secret values. This deliberately assumes matching
ProjectEnvironment login for default connections; project-local environments and
external login changes remain an attribution risk without a producer identity.
Settings/inventory changes invalidate generations; old connection generations
cannot repopulate new login data. No token fingerprint is retained.

Probe only with inference-enabled demand and stale 5h/7d coverage, at most one
billable operation and two external operations globally. Human task instructions
restrict this commit to demand-driven probes, overriding the design's idle
maintenance proposal. Preserve durable attempt-start floors, HTTP Retry-After,
exponential backoff and rejected-window reset on 429. Retain 429 windows while
recording refresh failure. HTTP 400/401/403 latch inference access; no model,
auth or immediate retry fallback. Request is the exact validated Haiku one-token
body, fixed endpoint and headers, redirects rejected, 30-second deadline, and
1 MiB response cap; generated content is discarded in chunks.

Only ProjectEnvironment CLAUDE_CODE_OAUTH_TOKEN is supported. Never launch a fresh
shell per refresh. Detect the exact config directory credentials-file presence
and macOS Keychain item metadata without reading passwords; stored OAuth is
explicitly unsupported pending a validated scoped credential-reader contract.
No credential writes, renewal, login UI or live test network calls are made.
Codex auth-file invalidation affects only Codex and preserves Claude access latches.

## Passive native xAI source

The global forwarding decorator observes only HTTPS api.x.ai inference response headers. It never reads request headers, keys or bodies and never buffers responses, including SSE. Model IDs are only present in native request bodies, so these observations explicitly use endpoint-specific unknown-model families and cannot satisfy a selected model query. No probes, metadata calls, Management API or credits are used.

The bounded channel uses try_send and drops telemetry when full or unavailable. Before service initialization no observation target exists. Registry and settings changes update the effective URL and credential-presence scope; responses retain their starting generation. Only the default trusted API URL is supported. No credential identity comparison is performed. Limits and remaining amounts are numeric snapshots without an invented window or reset; absolute RFC dates are accepted for reset headers, unknown reset formats remain unknown, and Retry-After seconds or absolute dates become a sanitized backoff hint; freshness ages normally and the panel labels recent usage only.

When SuperGrok and the API-key provider are both authenticated, the shared api.x.ai transport cannot attribute responses without forbidden credential inspection. Capture fails closed, clears prior numbers by advancing generation, and shows “cannot attribute: SuperGrok and API key both signed in”. Endpoint unknown-model buckets represent the latest xAI response, never an aggregate or average; xAI limits are per model and the request model is unavailable without reading the body. No model response header is verified by the spike, so no guessed header is trusted.
