"""ADR enforcement and declared insertion hooks."""

from dataclasses import dataclass, field
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

from riso_common import git, glob_matches, load_config, repository_root


@dataclass(frozen=True)
class Adr:
    """Customization metadata read from TOML front matter."""

    id: str
    title: str
    status: str
    review: str
    intent: str = ""
    owns: list[str] = field(default_factory=list)
    patches: list[str] = field(default_factory=list)
    hooks: list[str] = field(default_factory=list)
    watch: list[str] = field(default_factory=list)
    relies_on: list[str] = field(default_factory=list)
    contracts: list[str] = field(default_factory=list)
    obsolete_if: str = ""
    superseded_by: str = ""
    path: str = ""

    @property
    def live(self):
        return self.status in {"proposed", "active"}


@dataclass(frozen=True)
class Hook:
    """An anchored block to insert into one upstream file."""

    id: str
    file: str
    anchor: str
    position: str
    text: str


class HookAnchorError(ValueError):
    """Report a missing or ambiguous hook anchor."""

    def __init__(self, hook_id, kind):
        self.hook_id = hook_id
        self.kind = kind
        super().__init__(f"Hook {hook_id}: {kind} anchor")


def read_text(root, path, tree=None):
    if tree is None:
        return (Path(root) / path).read_text()
    return git(root, "show", f"{tree}:{path}")


def files_at(root, tree=None):
    if tree is None:
        return set(git(root, "ls-files", "--cached", "--others", "--exclude-standard").splitlines()) - {
            path for path in git(root, "ls-files", "--deleted").splitlines()
        }
    return set(git(root, "ls-tree", "-r", "--name-only", tree).splitlines())


def parse_adr(text, path):
    lines = text.splitlines()
    if not lines or lines[0] != "+++":
        raise ValueError("Front matter must begin with +++")
    try:
        end = lines.index("+++", 1)
    except ValueError as error:
        raise ValueError("Missing closing +++") from error
    metadata = tomllib.loads("\n".join(lines[1:end]))
    for name in ["id", "title", "status", "review"]:
        if not isinstance(metadata.get(name), str) or not metadata[name]:
            raise ValueError(f"Missing or invalid {name}")
    for name in ["owns", "patches", "hooks", "watch", "relies_on", "contracts"]:
        if not isinstance(metadata.get(name), list) or not all(isinstance(value, str) for value in metadata[name]):
            raise ValueError(f"Missing or invalid {name}")
    for name in ["intent", "obsolete_if", "superseded_by"]:
        if name in metadata and not isinstance(metadata[name], str):
            raise ValueError(f"Invalid {name}")
    if metadata["status"] not in {"proposed", "active", "superseded", "upstreamed", "dropped"}:
        raise ValueError("Invalid status")
    if metadata["review"] not in {"auto", "llm", "human"}:
        raise ValueError("Invalid review")
    filename = re.fullmatch(r"(\d{4})-[a-z0-9]+(?:-[a-z0-9]+)*\.md", Path(path).name)
    if not filename or metadata["id"] != f"ADR-{filename.group(1)}":
        raise ValueError("ADR id must match its filename number")
    if metadata["status"] in {"proposed", "active"} and not metadata.get("intent", "").strip():
        raise ValueError("Live ADR requires intent")
    if metadata["status"] == "superseded" and not metadata.get("superseded_by", "").strip():
        raise ValueError("Superseded ADR requires superseded_by")
    for heading in ["Context", "Decision", "Consequences", "Merge notes"]:
        if f"## {heading}" not in lines[end + 1:]:
            raise ValueError(f"Missing heading: {heading}")
    return Adr(**{name: value for name, value in metadata.items() if name in Adr.__dataclass_fields__ and name != "path"}, path=path)


def adr_paths(root, tree=None):
    return sorted(path for path in files_at(root, tree) if path.startswith("riso/adr/") and path.endswith(".md") and Path(path).name != "0000-template.md")


def load_adrs(repository_root, tree=None):
    """Read validated ADRs from the worktree or a git tree-ish."""
    result = [parse_adr(read_text(repository_root, path, tree), path) for path in adr_paths(repository_root, tree)]
    identifiers = [adr.id for adr in result]
    if len(identifiers) != len(set(identifiers)):
        raise ValueError("Duplicate ADR ids")
    return result


def load_hooks(repository_root, tree=None):
    """Read declared hooks from the worktree or a git tree-ish."""
    metadata = tomllib.loads(read_text(repository_root, "riso/hooks.toml", tree))
    hooks = []
    for entry in metadata.get("hooks", []):
        if not isinstance(entry, dict) or any(not isinstance(entry.get(name), str) for name in Hook.__dataclass_fields__):
            raise ValueError("Hook requires string id, file, anchor, position, text")
        hook = Hook(**entry)
        if hook.position not in {"before", "after", "end"} or not hook.id or not hook.text:
            raise ValueError(f"Invalid hook: {hook.id}")
        if Path(hook.file).is_absolute() or ".." in Path(hook.file).parts:
            raise ValueError(f"Hook path must be repository-relative: {hook.file}")
        if hook.position != "end" and not hook.anchor:
            raise ValueError(f"Hook requires anchor: {hook.id}")
        hooks.append(hook)
    if len({hook.id for hook in hooks}) != len(hooks):
        raise ValueError("Duplicate hook ids")
    return hooks


def apply_hooks_to_text(text, hooks_for_file):
    """Apply each shared insertion position as one ordered, idempotent block."""
    groups = {}
    for hook in hooks_for_file:
        groups.setdefault((hook.file, hook.anchor, hook.position), []).append(hook)
    for group in groups.values():
        hook = group[0]
        block = "".join(item.text.rstrip("\n") + "\n" for item in group)
        if hook.position == "end":
            if not text.endswith(block):
                text = text.rstrip("\n") + "\n" + block if text else block
            continue
        lines = text.splitlines(keepends=True)
        matches = [position for position, line in enumerate(lines) if hook.anchor in line]
        if len(matches) != 1:
            raise HookAnchorError(hook.id, "missing" if not matches else "ambiguous")
        position = matches[0] + (hook.position == "after")
        offset = sum(map(len, lines[:position]))
        if hook.position == "before" and text[:offset].endswith(block):
            continue
        if hook.position == "after" and text[offset:].startswith(block):
            continue
        if position and not lines[position - 1].endswith("\n"):
            text = text[:offset] + "\n" + text[offset:]
            offset += 1
        text = text[:offset] + block + text[offset:]
    return text


def is_hook_only(upstream_text, fork_text, hooks_for_file):
    """Return whether applying declared hooks exactly reproduces the fork text."""
    return apply_hooks_to_text(upstream_text, hooks_for_file) == fork_text


def changed_paths(root, base, head=None):
    arguments = ["diff", "--name-status", "-M", base]
    if head is not None:
        arguments.append(head)
    changes = []
    for line in git(root, *arguments).splitlines():
        status, *paths = line.split("\t")
        if status.startswith("R"):
            changes.extend([("D", paths[0]), ("A", paths[1])])
        elif paths:
            changes.append((status[0], paths[-1]))
    if head is None:
        changes.extend(("A", path) for path in git(root, "ls-files", "--others", "--exclude-standard").splitlines())
    return changes


def default_base(root, head, configuration):
    candidates = [configuration.branches.upstream_ref, f"origin/{configuration.branches.mirror}", configuration.branches.mirror]
    for candidate in candidates:
        if candidate in git(root, "for-each-ref", "--format=%(refname:short)").splitlines():
            return git(root, "merge-base", head, candidate).strip()
    raise ValueError(f"No upstream or mirror reference exists: {', '.join(candidates)}")


def check_repository(root, base, head=None, pr_body=None, sync_pr=False):
    """Collect all seven enforcement rules as (code, path, message) tuples."""
    errors = []
    def report(code, path, message):
        errors.append((code, path, message))
    configuration = load_config(root)
    adrs = []
    for path in adr_paths(root, head):
        try:
            adrs.append(parse_adr(read_text(root, path, head), path))
        except (ValueError, TypeError) as error:
            report("schema", path, str(error))
    identifiers = set()
    for adr in adrs:
        if adr.id in identifiers:
            report("schema", adr.path, f"Duplicate id: {adr.id}")
        identifiers.add(adr.id)
    try:
        hooks = load_hooks(root, head)
    except (ValueError, OSError, RuntimeError) as error:
        report("schema", "riso/hooks.toml", str(error))
        hooks = []
    live = [adr for adr in adrs if adr.live]
    existing = files_at(root, head)
    changes = changed_paths(root, base, head)
    changed = {path for _, path in changes}
    hook_map = {hook.id: hook for hook in hooks}
    referenced = {identifier for adr in live for identifier in adr.hooks}
    marker = configuration.policy.review_marker_lines
    readme = read_text(root, "README.md", head) if "README.md" in existing else ""
    marker_present = bool(marker) and readme.splitlines()[:len(marker)] == marker
    upstream_base = default_base(root, head or "HEAD", configuration)
    upstream_files = files_at(root, upstream_base)
    for status, path in changes:
        if path == "README.md" and marker_present:
            try:
                previous = read_text(root, path, base)
            except RuntimeError:
                previous = ""
            if "\n".join(readme.splitlines()[len(marker):]).lstrip("\n") == previous.rstrip("\n").lstrip("\n"):
                continue
        upstream_owned = path in upstream_files
        patterns = [pattern for adr in live for pattern in (adr.patches if upstream_owned else adr.owns)]
        covered = any(glob_matches(pattern, path) for pattern in patterns)
        file_hooks = [hook for hook in hooks if hook.file == path and hook.id in referenced]
        if not covered and upstream_owned and status != "D" and file_hooks:
            try:
                covered = is_hook_only(read_text(root, path, upstream_base), read_text(root, path, head), file_hooks)
            except (RuntimeError, OSError, HookAnchorError):
                covered = False
        if not covered:
            report("uncovered", path, "Changed path is not covered by a live ADR")
    modified = {path for status, path in changes if status != "A"}
    for adr in live:
        for pattern in adr.owns:
            if not any(glob_matches(pattern, path) for path in existing):
                report("stale", adr.path, f"owns matches no files: {pattern}")
        for pattern in adr.patches:
            if not any(glob_matches(pattern, path) for path in modified):
                report("stale", adr.path, f"patches matches no modified files: {pattern}")
        for identifier in adr.hooks:
            if identifier not in hook_map:
                report("stale", adr.path, f"Unknown hook: {identifier}")
        if (adr.owns or adr.patches or adr.hooks) and not adr.contracts:
            report("contracts", adr.path, "Live customization requires a contract")
        surface = any(any(glob_matches(pattern, path) for pattern in adr.owns + adr.patches) or any(hook_map[identifier].file == path for identifier in adr.hooks if identifier in hook_map) for path in changed)
        acknowledged = pr_body is not None and re.search(rf"^{re.escape(adr.id)}: unchanged\s+[—-]\s+\S.*$", pr_body, re.MULTILINE)
        if pr_body is not None and not sync_pr and surface and adr.path not in changed and not acknowledged:
            report("acknowledgement", adr.path, f"Update {adr.id} or acknowledge unchanged intent")
    for hook in hooks:
        count = sum(adr.hooks.count(hook.id) for adr in live)
        if count != 1:
            report("stale", "riso/hooks.toml", f"Hook {hook.id} must be referenced by exactly one live ADR")
    for path in sorted({hook.file for hook in hooks}):
        try:
            text = read_text(root, path, head)
            if apply_hooks_to_text(text, [hook for hook in hooks if hook.file == path]) != text:
                report("stale", path, "Hooks are not applied")
        except (OSError, RuntimeError, HookAnchorError) as error:
            report("stale", path, str(error))
    if marker_present:
        report("review_marker", "README.md", "Remove the review marker after a human has reviewed the change.")
    for adr in adrs:
        if adr.status in {"dropped", "upstreamed"}:
            if adr.hooks or any(glob_matches(pattern, path) for pattern in adr.owns + adr.patches for path in existing):
                report("lifecycle", adr.path, "Retired ADR must have no remaining surface or hooks")
    return errors


def command_check(arguments):
    root = repository_root()
    configuration = load_config(root)
    head = None if arguments.worktree else arguments.head
    base = arguments.base or default_base(root, arguments.head, configuration)
    body = Path(arguments.pr_body_file).read_text() if arguments.pr_body_file else None
    if body is None:
        print("Notice: acknowledgement skipped without --pr-body-file")
    errors = check_repository(root, base, head, body, arguments.sync_pr)
    for code, path, message in errors:
        if os.environ.get("GITHUB_ACTIONS") == "true":
            escaped = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
            escaped_path = path.replace("%", "%25").replace(",", "%2C").replace(":", "%3A").replace("\n", "%0A").replace("\r", "%0D")
            print(f"::error file={escaped_path}::{code}: {escaped}")
        else:
            print(f"{path}: {code}: {message}")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
            summary.write("\n| Code | Path | Message |\n| --- | --- | --- |\n")
            for error in errors:
                summary.write("| " + " | ".join(value.replace("|", "\\|").replace("\n", " ") for value in error) + " |\n")
    return int(bool(errors))


def command_hooks(arguments):
    root = repository_root()
    hooks = load_hooks(root)
    failed = False
    for path in sorted({hook.file for hook in hooks}):
        if getattr(arguments, "file", None) and path != arguments.file:
            continue
        try:
            text = read_text(root, path)
            applied = apply_hooks_to_text(text, [hook for hook in hooks if hook.file == path])
            if arguments.hook_command == "apply":
                if applied != text:
                    (root / path).write_text(applied)
            elif applied != text:
                print(f"{path}: hooks are not applied")
                failed = True
        except (OSError, HookAnchorError) as error:
            print(f"{path}: {error}")
            failed = True
    print(f"Hooks {arguments.hook_command}: {'failed' if failed else 'ok'}")
    return int(failed)


def command_contracts(arguments):
    root = repository_root()
    contracts = [{"adr": adr.id, "command": command} for adr in load_adrs(root) if adr.live for command in adr.contracts]
    if arguments.contract_command == "list":
        if arguments.json:
            print(json.dumps(contracts, indent=2))
        else:
            for contract in contracts:
                print(f"{contract['adr']}: {contract['command']}")
        return 0
    failures = 0
    for contract in contracts:
        result = subprocess.run(["bash", "-c", contract["command"]], cwd=root)
        print(f"{contract['adr']}: {'PASS' if result.returncode == 0 else 'FAIL'}: {contract['command']}")
        failures += result.returncode != 0
    print(f"Contracts: {len(contracts) - failures} passed, {failures} failed")
    return int(bool(failures))


def register(subparsers):
    """Register check, hooks, and contracts CLI commands."""
    check = subparsers.add_parser("check", help="Enforce customization ADRs")
    check.add_argument("--base")
    heads = check.add_mutually_exclusive_group()
    heads.add_argument("--head", default="HEAD")
    heads.add_argument("--worktree", action="store_true")
    check.add_argument("--pr-body-file")
    check.add_argument("--sync-pr", action="store_true")
    check.set_defaults(func=command_check)
    hooks = subparsers.add_parser("hooks", help="Apply or verify insertion hooks")
    hook_commands = hooks.add_subparsers(dest="hook_command", required=True)
    apply = hook_commands.add_parser("apply")
    apply.add_argument("--file")
    apply.set_defaults(func=command_hooks)
    hook_commands.add_parser("verify").set_defaults(func=command_hooks)
    contracts = subparsers.add_parser("contracts", help="List or run ADR contracts")
    contract_commands = contracts.add_subparsers(dest="contract_command", required=True)
    listing = contract_commands.add_parser("list")
    listing.add_argument("--json", action="store_true")
    listing.set_defaults(func=command_contracts)
    contract_commands.add_parser("run").set_defaults(func=command_contracts)
