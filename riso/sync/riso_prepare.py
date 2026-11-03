"""Prepare upstream synchronization branches and review requests."""

from datetime import datetime
import json
import re
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile

from riso_common import git, load_config, repository_root
from riso_adr import apply_hooks_to_text, is_hook_only, load_hooks


class Executor:
    def __init__(self, root, dry_run=False):
        self.root = Path(root)
        self.dry_run = dry_run
        self.commands = []
        self.failure_paths = []

    def run(self, *command, allowed_errors=()):
        command = list(map(str, command))
        self.commands.append(command)
        print(shlex.join(command))
        if self.dry_run:
            return ""
        result = subprocess.run(command, cwd=self.root, capture_output=True, text=True,
                                env={**os.environ, "GIT_MERGE_AUTOEDIT": "no"})
        if result.returncode and not any(message in result.stderr.lower() for message in allowed_errors):
            raise RuntimeError(f"{shlex.join(command)}: {result.stderr.strip()} {result.stdout.strip()}")
        return result.stdout

    def verify_hooks(self):
        self.commands.append(["python3", "riso/sync/riso_sync.py", "hooks", "verify"])
        if not self.dry_run:
            verify_hooks(self.root)

    def write(self, path, text):
        self.commands.append(["write", str(path), text])
        if not self.dry_run:
            Path(path).write_text(text)

    def body_command(self, command, body):
        if self.dry_run:
            return self.run(*command, "--body-file", "<generated-body>")
        with tempfile.TemporaryDirectory(prefix="riso-sync-") as directory:
            path = Path(directory) / "body.md"
            self.write(path, body)
            return self.run(*command, "--body-file", path)


def exists(root, reference):
    try:
        git(root, "rev-parse", "--verify", reference)
        return True
    except RuntimeError:
        return False


def require_clean(root):
    if git(root, "status", "--porcelain").strip():
        raise RuntimeError("The working tree must be clean before preparing a merge")


def verify_hooks(root):
    hooks = load_hooks(root)
    for path in sorted({hook.file for hook in hooks}):
        text = (Path(root) / path).read_text()
        if apply_hooks_to_text(text, [hook for hook in hooks if hook.file == path]) != text:
            raise RuntimeError(f"Hooks are not applied in {path}")


def sync_mirror(executor, config):
    branches = config.branches
    executor.run("git", "fetch", "upstream", branches.upstream_branch)
    # Dry runs cannot use FETCH_HEAD: it may describe an unrelated earlier fetch.
    reference = f"upstream/{branches.upstream_branch}" if executor.dry_run else "FETCH_HEAD"
    upstream = git(executor.root, "rev-parse", reference).strip()
    mirror = f"refs/remotes/origin/{branches.mirror}"
    if not exists(executor.root, mirror):
        raise RuntimeError(f"Mirror tracking ref {mirror} is missing; fetch origin first")
    try:
        git(executor.root, "merge-base", "--is-ancestor", mirror, upstream)
    except RuntimeError as error:
        raise RuntimeError("Refusing mirror push: upstream is not a fast-forward of origin mirror") from error
    executor.run("git", "push", "origin", f"{upstream}:refs/heads/{branches.mirror}")
    return 0


def merge(executor, target, message=None):
    executor.failure_paths = []
    command = ["git", "merge", "--no-ff"]
    if message:
        command.extend(["-m", message])
    command.append(target)
    try:
        executor.run(*command)
    except RuntimeError:
        conflicts = git(executor.root, "diff", "--name-only", "--diff-filter=U").splitlines()
        if not conflicts:
            raise
        executor.failure_paths = conflicts
        hooks = load_hooks(executor.root)
        resolutions = {}
        for path in conflicts:
            if path == "Cargo.lock":
                resolutions[path] = None
                continue
            file_hooks = [hook for hook in hooks if hook.file == path]
            try:
                original = git(executor.root, "show", f":1:{path}")
                ours = git(executor.root, "show", f":2:{path}")
                theirs = git(executor.root, "show", f":3:{path}")
                if file_hooks and is_hook_only(original, ours, file_hooks):
                    resolutions[path] = apply_hooks_to_text(theirs, file_hooks)
            except (RuntimeError, ValueError):
                # A missing stage or anchor makes automatic resolution unsafe.
                continue
        if set(conflicts) != set(resolutions):
            executor.run("git", "merge", "--abort")
            return False
        try:
            for path, text in resolutions.items():
                executor.run("git", "checkout", "--theirs", "--", path)
                if path == "Cargo.lock":
                    executor.run("cargo", "update", "--workspace")
                else:
                    executor.write(executor.root / path, text)
                executor.run("git", "add", "--", path)
            executor.verify_hooks()
            executor.run("git", "commit", "--no-edit")
        except (RuntimeError, OSError, ValueError):
            executor.run("git", "merge", "--abort")
            raise
        return True
    executor.verify_hooks()
    return True


def github_list(executor, kind, *arguments):
    output = executor.run("gh", kind, "list", "--state", "open", *arguments, "--json", "number")
    return json.loads(output or "[]")


def labels(executor, config, level, blocked=False):
    names = [config.labels.sync, f"{config.labels.level_prefix}{level}"]
    if blocked:
        names.append(config.labels.blocked)
    for name in names:
        executor.run("gh", "label", "create", name, "--force")
    return [argument for name in names for argument in ("--label", name)]


def publish_pull_request(executor, config, branch, title, body, level, blocked=False, eligible=False):
    label_arguments = labels(executor, config, level, blocked)
    existing = github_list(executor, "pr", "--head", branch, "--base", config.branches.product)
    if existing:
        number = str(existing[0]["number"])
        executor.body_command(["gh", "pr", "edit", number, "--title", title,
                               *["--add-label" if item == "--label" else item for item in label_arguments]], body)
    else:
        executor.body_command(["gh", "pr", "create", "--base", config.branches.product,
                               "--head", branch, "--title", title, *label_arguments], body)
        number = executor.run("gh", "pr", "view", branch, "--json", "number", "--jq", ".number").strip()
        if not number:
            if executor.dry_run:
                number = "<new-pr-number>"
            else:
                raise RuntimeError(f"Could not determine PR number for {branch}")
    if eligible and not blocked and not config.policy.shadow and level <= config.policy.auto_merge_max_level:
        executor.run("gh", "pr", "merge", "--auto", "--merge", number)
    else:
        executor.run("gh", "pr", "merge", "--disable-auto", number,
                     allowed_errors=("not enabled", "no auto-merge"))



def validate_plan(root, config, plan):
    def fields(value, required, location):
        if not isinstance(value, dict):
            raise ValueError(f"Malformed plan: {location} must be an object")
        for name, expected in required.items():
            if name not in value or type(value[name]) not in expected:
                raise ValueError(f"Malformed plan: {location}.{name} has a missing or invalid type")

    def strings(value, location):
        if not all(isinstance(item, str) for item in value):
            raise ValueError(f"Malformed plan: {location} must contain strings")

    def level(value, location):
        if type(value) is not int or value not in range(5):
            raise ValueError(f"Malformed plan: {location} must be a level from 0 to 4")

    def commit_sha(value, location):
        if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
            raise ValueError(f"Malformed plan: {location} must be a full commit SHA")
        git(root, "cat-file", "-e", f"{value}^{{commit}}")

    fields(plan, {"version": (int,), "generated_at": (str,), "upstream_repo": (str,),
                  "head": (str,), "upstream": (str,), "base": (str,), "jev": (dict,),
                  "commits": (list,), "auto": (dict,), "blocker": (dict, type(None)),
                  "summary": (dict,)}, "plan")
    if plan["version"] != 1:
        raise ValueError("Unsupported plan version")
    if datetime.fromisoformat(plan["generated_at"]).tzinfo is None:
        raise ValueError("Malformed plan: generated_at must include a timezone")
    fields(plan["jev"], {"enabled": (bool,), "model": (str,)}, "jev")
    for name in ("head", "upstream", "base"):
        commit_sha(plan[name], name)
    product = f"refs/heads/{config.branches.product}"
    if not exists(root, product):
        product = f"refs/remotes/origin/{config.branches.product}"
    if git(root, "rev-parse", "--verify", product).strip() != plan["head"]:
        raise ValueError("Stale plan: head does not equal the current product tip; regenerate the plan")
    if git(root, "merge-base", plan["head"], plan["upstream"]).strip() != plan["base"]:
        raise ValueError("Malformed plan: base is not the head/upstream merge base")
    records = plan["commits"]
    for record in records:
        fields(record, {"sha": (str,), "short": (str,), "subject": (str,),
                        "upstream_pr": (int, type(None)), "author": (str,), "date": (str,),
                        "files": (list,), "baseline_level": (int,), "level": (int,),
                        "reasons": (list,), "adrs": (list,), "conflicts": (dict,), "jev": (list,)}, "commit")
        commit_sha(record["sha"], "commit.sha")
        level(record["level"], "commit.level")
        level(record["baseline_level"], "commit.baseline_level")
        if record["level"] < record["baseline_level"]:
            raise ValueError("Malformed plan: commit level is below its baseline")
        strings(record["adrs"], "commit.adrs")
        for changed in record["files"]:
            fields(changed, {"status": (str,), "path": (str,), "old_path": (str, type(None))}, "file")
        fields(record["conflicts"], {"mechanical": (list,), "real": (list,)}, "conflicts")
        for name in ("mechanical", "real"):
            strings(record["conflicts"][name], f"conflicts.{name}")
        for reason in record["reasons"]:
            fields(reason, {"level": (int,), "code": (str,), "message": (str,),
                            "adr": (str, type(None)), "paths": (list,)}, "reason")
            level(reason["level"], "reason.level")
            strings(reason["paths"], "reason.paths")
            if reason["level"] > record["level"]:
                raise ValueError("Malformed plan: reason exceeds commit level")
        for decision in record["jev"]:
            fields(decision, {"adr": (str,), "model": (str,), "answers": (dict,),
                              "decision_level": (int,), "notes": (str,)}, "decision")
            level(decision["decision_level"], "decision.decision_level")
    expected_commits = git(root, "rev-list", "--reverse", "--first-parent",
                           f'{plan["base"]}..{plan["upstream"]}').splitlines()
    if [record["sha"] for record in records] != expected_commits:
        raise ValueError("Malformed plan: commits do not match the upstream range")
    auto = plan["auto"]
    fields(auto, {"target": (str, type(None)), "count": (int,), "max_level": (int,),
                  "auto_merge_eligible": (bool,)}, "auto")
    prefix = []
    blocker_record = None
    for record in records:
        if record["level"] >= 3:
            blocker_record = record
            break
        prefix.append(record)
    maximum = max((record["level"] for record in prefix), default=0)
    expected_target = prefix[-1]["sha"] if prefix else None
    eligible = maximum <= config.policy.auto_merge_max_level and not config.policy.shadow
    if (auto["target"], auto["count"], auto["max_level"], auto["auto_merge_eligible"]) != (expected_target, len(prefix), maximum, eligible):
        raise ValueError("Malformed plan: auto target, count, level, or eligibility does not match the commit prefix")
    blocker = plan["blocker"]
    if blocker is not None:
        fields(blocker, {"sha": (str,), "level": (int,), "mechanical_conflicts": (list,),
                         "real_conflicts": (list,)}, "blocker")
        strings(blocker["mechanical_conflicts"], "blocker.mechanical_conflicts")
        strings(blocker["real_conflicts"], "blocker.real_conflicts")
    if (blocker_record is None) != (blocker is None) or (blocker is not None and
            (blocker["sha"], blocker["level"]) != (blocker_record["sha"], blocker_record["level"])):
        raise ValueError("Malformed plan: blocker does not match the first blocked commit")
    expected_summary = {str(number): sum(record["level"] == number for record in records) for number in range(5)}
    if any(type(value) is not int for value in plan["summary"].values()) or plan["summary"] != expected_summary:
        raise ValueError("Malformed plan: summary does not match commits")


def record_auto_failure(executor, config, target, error):
    label_arguments = labels(executor, config, 3, True)
    paths = ", ".join(executor.failure_paths) or "Không xác định; xem lỗi bên dưới"
    body = (f"## Đồng bộ upstream bị chặn do lỗi ngoài dự kiến\n\n"
            f"Commit/target: `{target}`\n\nĐường dẫn: {paths}\n\nLỗi: {error}\n\n"
            f"Chạy `python3 riso/sync/riso_sync.py resolve {target}` để xử lý.\n").replace("@", "@\u200b")
    title = f"Sync upstream bị chặn → {target[:7]} (L3)"
    existing = github_list(executor, "issue", "--label", config.labels.blocked)
    if existing:
        command = ["gh", "issue", "edit", str(existing[0]["number"]), "--title", title,
                   *["--add-label" if argument == "--label" else argument for argument in label_arguments]]
    else:
        command = ["gh", "issue", "create", "--title", title, *label_arguments]
    executor.body_command(command, body)


def prepare(executor, config, plan, push=False, github=False):
    from riso_report import render_auto_body, render_blocked_body, render_waiting_comment

    validate_plan(executor.root, config, plan)
    if github:
        for kind in ("pr", "issue"):
            blocked = github_list(executor, kind, "--label", config.labels.blocked)
            if blocked:
                number = str(blocked[0]["number"])
                executor.body_command(["gh", kind, "comment", number], render_waiting_comment(plan, blocked[0]["number"]))
                print(f"Waiting on blocked {kind} {number}")
                return 0
    require_clean(executor.root)
    auto = plan["auto"]
    target = auto["target"]
    if target:
        branch = config.branches.sync_auto_branch
        try:
            executor.run("git", "checkout", "-B", branch, plan["head"])
            if not merge(executor, target, f"Merge upstream {target[:7]} (riso sync, max L{auto['max_level']})"):
                raise RuntimeError("Unexpected real conflict in auto prefix; merge aborted")
            body = render_auto_body(plan, config) if github else None
            if push:
                executor.run("git", "push", "--force-with-lease", "origin", branch)
        except (RuntimeError, OSError, ValueError) as error:
            if github:
                try:
                    record_auto_failure(executor, config, target, error)
                except (RuntimeError, OSError, ValueError) as reporting_error:
                    raise RuntimeError(f"Auto preparation failed: {error}; blocked issue failed: {reporting_error}") from error
            raise
        if github:
            publish_pull_request(executor, config, branch, f"Sync upstream → {target[:7]} (L{auto['max_level']})",
                                 body, auto["max_level"], eligible=auto["auto_merge_eligible"])
    blocker = plan.get("blocker")
    if blocker:
        branch = config.branches.sync_blocked_prefix + blocker["sha"][:7]
        start = config.branches.sync_auto_branch if target else plan["head"]
        executor.run("git", "checkout", "-B", branch, start)
        if executor.dry_run and blocker["real_conflicts"]:
            executor.run("git", "merge", "--no-ff", blocker["sha"])
            executor.run("git", "merge", "--abort")
            clean = False
        else:
            clean = merge(executor, blocker["sha"])
        body = render_blocked_body(plan, config)
        if not clean:
            if github:
                label_arguments = labels(executor, config, blocker["level"], True)
                body += f"\n\nChạy `python3 riso/sync/riso_sync.py resolve {blocker['sha']}` để xử lý xung đột.\n"
                executor.body_command(["gh", "issue", "create", "--title", f"Sync upstream bị chặn → {blocker['sha'][:7]}", *label_arguments], body)
        else:
            if push:
                executor.run("git", "push", "--force-with-lease", "origin", branch)
            if github:
                publish_pull_request(executor, config, branch, f"Sync upstream → {blocker['sha'][:7]} (L{blocker['level']})",
                                     body, blocker["level"], blocked=True)
    return 0


def resolve(executor, config, target, finish=False):
    target = git(executor.root, "rev-parse", "--verify", f"{target}^{{commit}}").strip()
    branch = config.branches.sync_blocked_prefix + target[:7]
    if not finish:
        require_clean(executor.root)
        start = f"origin/{config.branches.sync_auto_branch}"
        if not exists(executor.root, start):
            start = f"origin/{config.branches.product}"
        executor.run("git", "checkout", "-B", branch, start)
        try:
            executor.run("git", "merge", "--no-ff", target)
        except RuntimeError:
            if not git(executor.root, "diff", "--name-only", "--diff-filter=U").strip():
                raise
        print(f"Giải quyết xung đột, git add và git commit, rồi chạy: python3 riso/sync/riso_sync.py resolve {target} --finish")
        return 0
    if git(executor.root, "branch", "--show-current").strip() != branch:
        raise RuntimeError(f"Expected blocked branch {branch}")
    git(executor.root, "diff", "--check")
    git(executor.root, "diff", "--cached", "--check")
    if git(executor.root, "diff", "--name-only", "--diff-filter=U").strip():
        raise RuntimeError("Unresolved merge conflicts remain")
    for path in git(executor.root, "ls-files", "-z").split("\0"):
        if path and (executor.root / path).is_file():
            if any(line.startswith(b"<<<<<<<") for line in (executor.root / path).read_bytes().splitlines()):
                raise RuntimeError(f"Conflict marker remains in {path}")
    executor.verify_hooks()
    require_clean(executor.root)
    if exists(executor.root, "MERGE_HEAD"):
        raise RuntimeError("Commit the resolved merge before --finish")
    git(executor.root, "merge-base", "--is-ancestor", target, "HEAD")
    executor.run("git", "push", "--force-with-lease", "origin", branch)
    publish_pull_request(executor, config, branch, f"Sync upstream → {target[:7]} (L3)",
                         f"Đã xử lý thủ công upstream `{target}`.\n", 3, blocked=True)
    return 0


def dispatch(arguments):
    try:
        root = repository_root()
        config = load_config(root)
        executor = Executor(root, arguments.dry_run)
        if arguments.command == "sync-mirror":
            return sync_mirror(executor, config)
        if arguments.command == "prepare":
            with Path(arguments.plan).open() as source:
                plan = json.load(source)
            return prepare(executor, config, plan, arguments.push, arguments.github)
        return resolve(executor, config, arguments.sha, arguments.finish)
    except (RuntimeError, ValueError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 1


def register(subparsers):
    mirror = subparsers.add_parser("sync-mirror", help="Fast-forward the upstream mirror")
    preparation = subparsers.add_parser("prepare", help="Prepare synchronization branches and review requests")
    preparation.add_argument("--plan", required=True, help="Version 1 plan JSON file")
    preparation.add_argument("--push", action="store_true", help="Push prepared branches")
    preparation.add_argument("--github", action="store_true", help="Create or update GitHub review requests")
    resolution = subparsers.add_parser("resolve", help="Resolve a blocked upstream merge locally")
    resolution.add_argument("sha", help="Blocked upstream commit")
    resolution.add_argument("--finish", action="store_true", help="Verify and publish a resolved merge")
    for parser in (mirror, preparation, resolution):
        parser.add_argument("--dry-run", action="store_true", help="Log side effects without executing them")
        parser.set_defaults(func=dispatch)
