"""Classify upstream commits against the fork's declared customizations."""

from datetime import datetime, timezone
import json
from pathlib import Path, PurePosixPath
import re
import tomllib

from riso_common import git, glob_matches, load_config, repository_root
from riso_adr import load_adrs, load_hooks, files_at, read_text, is_hook_only, apply_hooks_to_text, HookAnchorError
from riso_jev import build_state, evaluate, JevError, StateTooLarge


def changes(root, before, after):
    fields = git(root, "diff", "-M", "--name-status", "-z", before, after).rstrip("\0").split("\0")
    result = []
    position = 0
    while position < len(fields) and fields[position]:
        status = fields[position]
        position += 1
        old_path = None
        path = fields[position]
        position += 1
        if status.startswith(("R", "C")):
            old_path = path
            path = fields[position]
            position += 1
        result.append({"status": status[0], "path": path, "old_path": old_path})
    return result


def matches(patterns, path):
    return any(glob_matches(pattern, path) for pattern in patterns)


def marker_only(root, base, head, path, configuration):
    if path != "README.md":
        return False
    previous = read_text(root, path, base)
    current = read_text(root, path, head)
    marker = configuration.policy.review_marker_lines
    lines = current.splitlines()
    return lines[:len(marker)] == marker and "\n".join(lines[len(marker):]).strip("\n") == previous.strip("\n")


def custom_surface(root, base, head, configuration):
    adrs = [adr for adr in load_adrs(root, head) if adr.live]
    existing = files_at(root, head)
    hooks = load_hooks(root, head) if "riso/hooks.toml" in existing else []
    base_files = files_at(root, base)
    hook_groups = {path: [hook for hook in hooks if hook.file == path] for path in {hook.file for hook in hooks}}
    hook_only = set()
    patched = set()
    for change in changes(root, base, head):
        path = change["old_path"] or change["path"]
        if change["status"] not in ("M", "D", "R"):
            continue
        if path in existing and path in base_files:
            if marker_only(root, base, head, path, configuration):
                continue
            if path in hook_groups:
                try:
                    if is_hook_only(read_text(root, path, base), read_text(root, path, head), hook_groups[path]):
                        hook_only.add(path)
                        continue
                except HookAnchorError:
                    # A broken existing hook must remain part of the patched surface.
                    patched.add(path)
                    continue
        patched.add(path)
    workspace = tomllib.loads(read_text(root, "Cargo.toml", head)).get("workspace", {}).get("dependencies", {}) if "Cargo.toml" in existing else {}
    dependencies = {}
    symbols = {}
    for adr in adrs:
        owned = sorted(path for path in existing if matches(adr.owns, path))
        crate_directories = {}
        for manifest in (path for path in owned if PurePosixPath(path).name == "Cargo.toml"):
            metadata = tomllib.loads(read_text(root, manifest, head))
            for name, entry in metadata.get("dependencies", {}).items():
                if not isinstance(entry, dict):
                    continue
                parent = PurePosixPath(manifest).parent
                if entry.get("workspace"):
                    entry = workspace.get(name, {})
                    parent = PurePosixPath(".")
                if not isinstance(entry, dict) or "path" not in entry:
                    continue
                directory = (Path(root) / str(parent) / entry["path"]).resolve()
                try:
                    relative = directory.relative_to(Path(root).resolve()).as_posix()
                except ValueError:
                    continue
                if f"{relative}/Cargo.toml" not in existing:
                    continue
                crate_directories[name.replace("-", "_")] = relative
        for reference in adr.relies_on:
            if re.fullmatch(r"[a-z_][a-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)+", reference):
                crate, *segments = reference.split("::")
                symbols.setdefault((adr.id, crate), set()).add(segments[-1])
                if crate not in crate_directories:
                    entry = workspace.get(crate, workspace.get(crate.replace("_", "-"), {}))
                    if isinstance(entry, dict) and "path" in entry:
                        directory = (Path(root) / entry["path"]).resolve()
                        if directory.is_relative_to(Path(root).resolve()):
                            relative = directory.relative_to(Path(root).resolve()).as_posix()
                            if f"{relative}/Cargo.toml" in existing:
                                crate_directories[crate] = relative
        for path in (path for path in owned if path.endswith(".rs")):
            text = read_text(root, path, head)
            for crate, imported in re.findall(r"\buse\s+(?:::)?([A-Za-z_][A-Za-z0-9_]*)\s*::\s*([^;]+);", text, re.DOTALL):
                if crate in crate_directories:
                    names = re.findall(r"\b[A-Za-z_][A-Za-z0-9_]*\b", re.sub(r"\bas\s+\w+", "", imported))
                    symbols.setdefault((adr.id, crate), set()).update(name for name in names if name not in ("self", "super"))
        for crate, directory in crate_directories.items():
            dependencies[(adr.id, crate)] = directory
    return adrs, hooks, patched, hook_only, dependencies, symbols


def merge_conflicts(root, head, commit):
    # The shared helper raises on merge-tree's expected status 1 and discards stdout.
    # Capture it through a git alias whose shell wrapper normalizes only that status.
    alias = '!f() { git merge-tree --write-tree --name-only -z "$@"; result=$?; if [ "$result" -eq 1 ]; then exit 0; else exit "$result"; fi; }; f'
    output = git(root, "-c", f"alias.riso-merge-conflicts={alias}", "riso-merge-conflicts", head, commit)
    fields = output.split("\0")
    paths = []
    for path in fields[1:]:
        if not path:
            break
        paths.append(path)
    return paths


def build_plan(root, head="HEAD", upstream=None, no_jev=False, configuration=None, client=None):
    configuration = configuration or load_config(root)
    head = git(root, "rev-parse", head).strip()
    upstream = git(root, "rev-parse", upstream or configuration.branches.upstream_ref).strip()
    base = git(root, "merge-base", head, upstream).strip()
    commits = git(root, "rev-list", "--reverse", "--first-parent", f"{base}..{upstream}").splitlines()
    adrs, hooks, patched, hook_only, dependencies, symbols = custom_surface(root, base, head, configuration)
    hook_groups = {path: [hook for hook in hooks if hook.file == path] for path in {hook.file for hook in hooks}}
    seen_conflicts = set()
    records = []
    for commit in commits:
        subject, author, date = git(root, "show", "-s", "--format=%s%n%an%n%aI", commit).rstrip("\n").split("\n", 2)
        parent = git(root, "rev-parse", f"{commit}^").strip()
        files = changes(root, parent, commit)
        reasons = []
        affected = set()
        relevant = {}
        def reason(level, code, paths, adr=None, message=None):
            reasons.append({"level": level, "code": code, "message": message or code.replace("_", " "),
                            "adr": adr.id if adr else None, "paths": sorted(set(paths))})
            if adr:
                affected.add(adr.id)
                relevant.setdefault(adr.id, set()).update(paths)
        existing = files_at(root, commit) if hook_groups else set()
        applicable_hooks = set()
        for change in files:
            paths = {change["path"]}
            if change["old_path"]:
                paths.add(change["old_path"])
            for path in paths:
                patched_owners = [adr for adr in adrs if matches(adr.patches, path) or
                                  path in patched and (matches(adr.owns, path) or
                                  any(hook.id in adr.hooks for hook in hook_groups.get(path, [])))]
                if path in patched or patched_owners:
                    for adr in patched_owners or [None]:
                        reason(2, "patched_file", [path], adr)
                        if change["status"] in ("D", "R"):
                            reason(4, "surface_removed", [path], adr)
                for adr in adrs:
                    if matches(adr.watch, path):
                        reason(2, "watch", [path], adr)
                        if change["status"] in ("D", "R"):
                            reason(4, "surface_removed", [path], adr)
                if path in hook_groups:
                    try:
                        if path not in existing:
                            raise HookAnchorError(hook_groups[path][0].id, "missing")
                        apply_hooks_to_text(read_text(root, path, commit), hook_groups[path])
                        applicable_hooks.add(path)
                        level, code = 1, "hook_file"
                    except HookAnchorError:
                        level, code = 4, "hook_anchor"
                    owners = [adr for adr in adrs if any(hook.id in adr.hooks for hook in hook_groups[path])]
                    for adr in owners or [None]:
                        reason(level, code, [path], adr)
                for (identifier, crate), directory in dependencies.items():
                    if path.startswith(directory + "/"):
                        adr = next(adr for adr in adrs if adr.id == identifier)
                        reason(1, "dependency_crate", [path], adr)
                        diff = git(root, "diff", "--unified=0", parent, commit, "--", path)
                        changed_lines = "\n".join(line[1:] for line in diff.splitlines() if line.startswith(("+", "-")) and not line.startswith(("+++", "---")))
                        for symbol in symbols.get((identifier, crate), set()):
                            escaped = re.escape(symbol)
                            if re.search(rf"\b(?:fn|struct|enum|trait|type|const|static|mod|macro_rules!)\s+{escaped}\b|\bimpl\b[^\n]*\b{escaped}\b", changed_lines):
                                reason(2, "symbol_definition", [path], adr)
        mechanical, real = [], []
        for path in (merge_conflicts(root, head, commit) if head != base else []):
            if path in seen_conflicts:
                continue
            seen_conflicts.add(path)
            if path == "Cargo.lock" or (path in hook_only and path in applicable_hooks):
                mechanical.append(path)
                reason(1, "mechanical_conflict", [path])
            else:
                real.append(path)
                reason(3, "conflict", [path])
        for adr in adrs:
            if adr.review == "human" and any(item["adr"] == adr.id and item["code"] in ("symbol_definition", "patched_file", "watch") for item in reasons):
                reason(3, "review_policy", relevant[adr.id], adr)
        baseline = max((item["level"] for item in reasons), default=0)
        jev_records = []
        if not no_jev and (baseline == 2 or baseline == 1 and any(item["code"] == "dependency_crate" for item in reasons)):
            targets = [adr for adr in adrs if adr.id in affected and
                       (baseline == 2 or any(item["adr"] == adr.id and item["code"] == "dependency_crate" for item in reasons))]
            if not targets:
                reason(3, "jev_unavailable", [], message="No ADR describes the changed custom surface")
            for adr in targets:
                paths = sorted(relevant[adr.id])
                try:
                    state = build_state(adr, subject, [item for item in files if item["path"] in paths or item["old_path"] in paths],
                                        git(root, "diff", parent, commit, "--", *paths),
                                        git(root, "diff", base, head, "--", *paths), configuration)
                    level, notes, decisions = evaluate(adr, state, baseline, configuration, client)
                    jev_records.extend(decisions)
                    reason(level, notes if notes != "safe" else "jev_safe", paths, adr)
                except StateTooLarge as error:
                    reason(3, "jev_state_too_large", paths, adr, str(error))
                except JevError as error:
                    reason(3, "jev_unavailable", paths, adr, str(error))
        upstream_pr = re.search(r"\(#(\d+)\)$", subject)
        records.append({"sha": commit, "short": commit[:7], "subject": subject,
                        "upstream_pr": int(upstream_pr.group(1)) if upstream_pr else None,
                        "author": author, "date": date, "files": files, "baseline_level": baseline,
                        "level": max((item["level"] for item in reasons), default=0), "reasons": reasons,
                        "adrs": sorted(affected), "conflicts": {"mechanical": mechanical, "real": real}, "jev": jev_records})
    prefix = []
    blocker = None
    for record in records:
        if record["level"] >= 3:
            blocker = {"sha": record["sha"], "level": record["level"],
                       "mechanical_conflicts": record["conflicts"]["mechanical"], "real_conflicts": record["conflicts"]["real"]}
            break
        prefix.append(record)
    maximum = max((record["level"] for record in prefix), default=0)
    return {"version": 1, "generated_at": datetime.now(timezone.utc).isoformat(),
            "upstream_repo": configuration.branches.upstream_repo, "head": head, "upstream": upstream, "base": base,
            "jev": {"enabled": not no_jev, "model": configuration.jev.model}, "commits": records,
            "auto": {"target": prefix[-1]["sha"] if prefix else None, "count": len(prefix), "max_level": maximum,
                     "auto_merge_eligible": maximum <= configuration.policy.auto_merge_max_level and not configuration.policy.shadow},
            "blocker": blocker, "summary": {str(level): sum(record["level"] == level for record in records) for level in range(5)}}


def command_plan(arguments):
    from riso_report import render_auto_body, render_blocked_body
    root = repository_root()
    configuration = load_config(root)
    plan = build_plan(root, arguments.head, arguments.upstream, arguments.no_jev, configuration)
    Path(arguments.out).write_text(json.dumps(plan, indent=2) + "\n")
    if arguments.body_auto:
        Path(arguments.body_auto).write_text(render_auto_body(plan, configuration))
    if arguments.body_blocked:
        Path(arguments.body_blocked).write_text(render_blocked_body(plan, configuration))
    print(json.dumps(plan["summary"]))
    return 0


def register(subparsers):
    parser = subparsers.add_parser("plan", help="Classify an upstream synchronization range")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--upstream")
    parser.add_argument("--out", required=True)
    parser.add_argument("--body-auto")
    parser.add_argument("--body-blocked")
    parser.add_argument("--no-jev", action="store_true")
    parser.set_defaults(func=command_plan)
