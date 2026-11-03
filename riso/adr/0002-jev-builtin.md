+++
id = "ADR-0002"
title = "Built-in Jev decision support"
status = "active"
review = "human"
intent = "Ship Jev decision tools inside the Zed binary, available to native and MCP-capable ACP agents without installing an extension or writing a settings entry."
owns = ["crates/jev/**"]
patches = ["Cargo.toml", "Cargo.lock", "crates/zed/Cargo.toml", "assets/settings/default.json", "crates/zed/src/main.rs", "crates/project/tests/integration/context_server_store.rs", "crates/zed_actions/src/lib.rs", "crates/agent_ui/src/context_server_configuration.rs", "crates/settings_ui/src/pages/mcp_servers_page.rs", "crates/agent_ui/src/agent_configuration/configure_context_server_modal.rs"]
hooks = ["jev-init"]
watch = ["crates/workspace/src/notifications.rs", "crates/workspace/src/workspace.rs", "assets/settings/default.json", "crates/settings_content/src/project.rs", "crates/settings_content/src/merge_from.rs", "crates/settings_macros/src/settings_macros.rs", "crates/remote_server/src/headless_project.rs", "crates/util/src/util.rs", "crates/fs/src/fs.rs", "crates/paths/src/paths.rs", "crates/project/src/context_server_store.rs", "crates/project/src/context_server_store/registry.rs", "crates/project/src/project_settings.rs", "crates/node_runtime/**", "crates/agent_servers/src/acp.rs", "crates/agent_servers/src/acp/**", "crates/agent/src/agent.rs"]
relies_on = ["Project::init registers project settings before jev::init", "ContextServerDescriptorRegistry supports native descriptors and their configure-modal schema", "main.rs signals login-shell completion explicitly to Jev independently of NodeRuntime options", "paths::data_dir is absolute", "ProjectSettings preserves default context server entries", "ContextServerStore::resolve_all_context_server_settings reads merged default settings with and without folders", "configured_server_ids includes the enabled default jev entry", "ContextServerConfiguration::from_settings resolves registry descriptors", "Native ContextServerRegistry exposes store tools", "ACP v1 mcp_servers_for_project and v2 session_context forward descriptor command path arguments and environment"]
contracts = ["node --test crates/jev/server.test.cjs", "cargo test -p jev", "cargo check -p jev -p zed"]
obsolete_if = "Upstream ships equivalent built-in Jev decision support with zero-install native and ACP discovery."
superseded_by = ""
+++

## Context

Jev must be installed with this fork, not an extension requiring a dev-install or
registry publishing step. The previously implemented MCP protocol, tools, validation,
key redaction and retries remain useful and are preserved in the binary-owned crate.

## Decision

Own `crates/jev`, embed the dependency-free Node server, and register its descriptor
through the existing ContextServerDescriptorRegistry. A single anchored startup call
after Project::init supplies the existing NodeRuntime and filesystem. The descriptor
provides the settings schema/defaults and writes the script under Zed's data directory
after credential validation, with content-hashed filenames and atomic writes. Legacy
unversioned scripts are removed on rewrite; other hashed versions are retained
because cached ACP commands can reference them, preventing cross-build deletion races. Startup always registers the descriptor and starts the MCP server, including without
a key. This makes Configure schema fields and tools discoverable before setup.
Missing-key tool calls return UI setup instructions to the agent without a Zed
notification or error-level log. Existing store settings observation restarts the
server with a new environment after Configure saves; Zed itself need not restart.
There is no key-dependent descriptor registration/unregistration or MISSING_KEY
command error.
The notification monitor watches sanitized JSONL failure events and dispatches application
notifications to workspaces, including failures from externally spawned ACP MCPs.
Only failures after an HTTP attempt emit events (network/timeout, non-2xx, retry
exhaustion, invalid response); local input-validation errors do not. Node writes
asynchronously with a 256 KiB event-file cap, fixed short messages and no credentials,
state, request body or response details; write failures only emit one generic stderr
line per process. Rust reconstructs actionable messages from validated kinds/statuses,
ignores unknown event fields and throttles each kind for 60 seconds.

Settings -> MCP Servers (also reached from the Agent panel's settings entry) showed no
Configure button for extension/descriptor-provided servers, so a built-in key could
only be typed into settings.json. A narrow, generic upstream patch adds the
`agent::ConfigureContextServer { id }` action in zed_actions, a workspace handler in
agent_ui that opens the existing ConfigureContextServerModal for that id, and a
Configure button on the settings page for provided servers that dispatches the action
through the original workspace window. The gear is shown only while the server's
descriptor is registered. Because that button newly exposes the modal for servers that
are already configured, the modal is patched to prefill existing extension settings
(a non-empty object) over the defaults and to keep the existing `remote` flag;
otherwise confirming would replace e.g. a working token with the default placeholder.
Jev's defaults contain only `api_key`, with optional overrides as JSONC comments, so
saving does not pin the current model/endpoint/timeout into user settings. The modal
is an upstream JSON editor, so an existing key is shown there in plain text.

Patch the workspace/dependency manifests and lockfile to link the crate into Zed,
and add one enabled entry in assets/settings/default.json. Merged defaults reach
ProjectSettings and ContextServerStore even without user settings or worktrees;
configured_server_ids therefore exposes Jev to ACP as well as the native registry.
No agent_servers or project implementation patches are needed. Remove the prior
extension entirely, including its workspace and lockfile records, to avoid duplicate IDs.
The registry's existing Extension configuration variant is used as a descriptor-backed
configuration; it does not require an extension installation or a wasm dependency.

Read context_servers.jev.settings with the same visible-worktree precedence as the
former extension. A non-empty settings key wins over inherited JEV_API_KEY. Wait for
the explicit main.rs login-shell completion signal before reading the environment,
independently of node.path and ignore_system_version. Only fall back to inherited
JEV_API_KEY for the exact default TypeSafe endpoint; custom endpoints need an explicit
settings key. Then resolve Node and forward the validated key explicitly for ACP; JEV_SETTINGS excludes api_key. The command formatter
redacts KEY suffixes. Existing ACP protocol debug logs can still contain MCP secrets.

Keep Noul, Score, Choice and image Box tools, structured descriptions, finite/depth/body
limits, typed response validation, JSON-RPC fixes, and three retries for 429/529 within
the total deadline honoring Retry-After. HTTP 400/413/422 return at most 500 characters
of detail/code with keys of at least eight characters redacted; other bodies are omitted.

## Consequences

Jev is available out of the box; only credentials need configuring. Node provisioning
uses existing application settings and may need network access on first use. Agent
MCP capabilities, permissions and disabled-AI settings still apply. Jev is local-only:
on remote projects only the native agent gets local Jev; ACP does not, and remote:true
is unsupported. Start a new agent session if Jev is missing right after first launch.
Each project window runs one Jev Node process, with or without a key, because the
tools must be discoverable before setup; ACP sessions add their own MCP processes.
Every request of an MCP-enabled native profile and every MCP-capable ACP session also
carries the three Jev tool definitions (about 7.5 KB of JSON, roughly 2k tokens), keyless
users included; disable the entry to avoid both costs.
Ask/Minimal profiles do not enable MCP tools; use Write or a custom MCP-enabled profile. Uninstall only removes the user entry,
so disable with enabled:false and settings:{} instead. The enabled default can be overridden with enabled=false. Inline PNG/JPEG assets
require an image-capable model; multipart and long-context opt-in remain outside scope.
Supplied context/images go to TypeSafe. Trusted project settings replace the Jev entry rather than deep-merging it. They
cannot redirect an inherited key to another endpoint; explicit settings keys and
endpoints remain the user's responsibility. Trust reviewed worktrees/endpoints only.

## Merge notes

Reapply the declared jev-init hook after Project::init, preserve the default settings
entry and crate dependencies, and verify default-based ACP discovery when changing
settings merging or server registries. Keep the README review marker for manual human
removal after review. This ADR supersedes its earlier extension packaging decision
without creating a second customization ID.

The upstream first-worktree test filters out only the built-in jev ID from its
custom-server assertion; no default entry or upstream profile behavior is changed.
main.rs adds a second completion receiver sent by the same login-shell loader so the
existing NodeRuntime receiver remains unchanged and the init hook remains one call.

MCP initialization includes Jev identity/use instructions and every tool description
starts with "Jev (TypeSafe System One) decision support:". The current native MCP
InitializeResponse type ignores instructions, so native identity comes through tool
names/descriptions; external clients that support instructions receive them too.
No upstream parser/profile/store logic is patched.

The Configure patch touches four upstream files and is not Jev-specific: the
zed_actions action struct, its registration in agent_ui context_server_configuration
init (before the extension-events early return), the settings_ui button for provided
servers (hidden without a registered descriptor), and configure_context_server_modal.rs,
where ConfigurationTarget/ConfigurationSource::Extension carry the existing settings and
`remote`, `extension_settings_input` merges existing settings over the defaults, and
`output()` keeps `remote`. On modal conflicts keep those three behaviors and the test
configuring_existing_extension_server_preserves_its_settings. On conflicts, keep the action name/field, keep dispatching through
the active workspace's focus handle, and keep the modal's existing
show_modal_for_existing_server entry point. Drop the patch if upstream adds its own
Configure for extension-provided servers on the settings page.
