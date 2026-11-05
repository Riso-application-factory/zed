+++
id = "ADR-0005"
title = "Connect Antigravity through ACP v1"
status = "proposed"
review = "human"
intent = "Restore Antigravity threads and catalog discovery while its initialize response uses legacy fields."
owns = []
patches = ["crates/agent_servers/src/acp.rs"]
hooks = []
watch = ["crates/agent_servers/src/custom.rs", "crates/agent_servers/src/acp/v1.rs", "crates/agent_servers/src/acp/v2.rs", "crates/crew_model_catalog/src/upstream.rs", "crates/project/src/agent_registry_store.rs"]
relies_on = ["Antigravity registry identity is antigravity-acp", "CustomAgentServer and catalog probes share acp::connect", "The existing v1 transport offers protocolVersion 1 without rewriting responses"]
contracts = ["cargo test -p agent_servers selection_tests", "cargo test -p crew_model_catalog", "./script/clippy -p agent_servers -p crew_model_catalog", "cargo fmt --all -- --check", "cargo check -p zed"]
obsolete_if = "The vendor sends a valid v2 response, or upstream adds compatibility."
superseded_by = ""
+++

## Context

Antigravity 1.4.5 selects protocolVersion 2 but returns legacy agentInfo,
agentCapabilities and auth fields. The v2 SDK requires info and rejects it before
threads or model discovery can connect. The local upstream investigation report
contains the schema diagnosis but no exact sanitized initialize JSON fixture.

## Decision

Use option (c), a registry-ID exception at the shared selection seam in
`crates/agent_servers/src/acp.rs:124`. No per-agent protocol setting or public
connection-policy API exists: `CustomAgentServer::connect` delegates directly to
this selector (`custom.rs:274`), which alone chooses v2 via the global beta flag.
A Crew wrapper would need a new upstream API or duplicate launch logic; the
exception requires only one extra condition and leaves the existing v1 transport
responsible for the handshake. The actual registry ID is `antigravity-acp`, not
`antigravity`. Custom aliases are intentionally outside this exception.

Normal threads and `crew_model_catalog/src/upstream.rs:171` use the same
CustomAgentServer path. No catalog change is necessary. Quota's separate v1
transport is untouched. No malformed-response retry or JSON normalization is
introduced. Connection failures still propagate through the existing UI path.

## Consequences

Antigravity does not receive v2 requests even when ACP beta is enabled. All other
IDs retain existing selection and mismatch fallback behavior. The subprocess
selection regression asserts the exact offered version and concrete connection
type, using a synthetic legacy-shaped v2 response matching the report's diagnosis.
Remove the exception when either obsolete condition is met; monitor registry
Antigravity releases and upstream ACP initialization compatibility changes.

## Merge notes

Retain the scoped condition at the shared selector when rebasing; do not move it
into catalog or quota code. No hook is needed because selection is already shared.
