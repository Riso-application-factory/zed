+++
id = "ADR-0001"
title = "Upstream synchronization pipeline"
status = "active"
review = "human"
intent = "Keep the customized fork synchronized through reviewed pull requests with explicit customization contracts and risk-based upstream triage."
owns = ["riso/**", ".github/workflows/riso_*.yml"]
patches = []
hooks = []
watch = []
relies_on = []
contracts = ["python3 -m unittest discover -s riso/sync/tests -t riso/sync"]
obsolete_if = ""
superseded_by = ""
+++

## Context

The fork must preserve customizations while regularly importing upstream changes.
ADRs describe ownership, direct patches, hooks, assumptions, and executable contracts.

## Decision

All changes reach the product branch through pull requests. Deterministic signals classify
upstream commits as L0 (unrelated), L1 (nearby or mechanical), L2 (customization surface),
L3 (human review or real conflict), or L4 (removed surface or restructuring).
Jev decides semantic risk and may only raise the deterministic level; Claude Code is used
only for generation, never as the decision layer. Contracts and ADR checks enforce the
customization boundary. Hooks are explicit, anchored, idempotent insertions. Hooks sharing
a file, anchor, and position form one contiguous block in declaration order. Coverage uses
the upstream merge-base tree to distinguish fork-owned files from upstream patches; the
comparison base only selects the changes to inspect.

The pipeline opens an auto prefix PR and stops at the first L3/L4 blocker. L2 waits for
human review under the initial L1 auto-merge threshold. Shadow mode is enabled initially:
PRs are opened but auto-merge is never enabled until shadow mode is deliberately disabled.

## Consequences

The GitHub App riso-tech-bot provides write access: GITHUB_TOKEN cannot push workflow-file
changes, and pull requests created with GITHUB_TOKEN do not trigger normal CI workflows.
App tokens are short-lived, are not tied to a person, and trigger CI for the App's PRs.
The App is owned by riso-tech and installed on Riso-application-factory with Contents,
Pull requests, Workflows, and Issues write permissions. Organization secrets RISO_BOT_APP_ID
and RISO_BOT_PRIVATE_KEY supply its credentials for minting installation tokens.
Missing Jev credentials or uncertain decisions escalate to human review.

## Merge notes

Keep policy thresholds aligned with the pinned Jev model version. Review this ADR whenever
the infrastructure or its contracts change; remove the README marker manually after review.
