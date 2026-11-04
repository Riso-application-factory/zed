"""Prepare Claude Code inputs and verify its output; Claude generates text and edits, never decisions."""

import json
from pathlib import Path
import re
import subprocess
import sys

from riso_common import git, load_config, repository_root
from riso_adr import apply_hooks_to_text, load_hooks

CONFLICT_MARKERS = (b"<<<<<<<", b"|||||||", b">>>>>>>")
VERDICTS = ("safe", "review", "risky")
REVIEW_SCHEMA = {
    "type": "object",
    "properties": {
        "commits": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "short": {"type": "string"},
                    "verdict": {"type": "string", "enum": list(VERDICTS)},
                    "reason": {"type": "string"},
                },
                "required": ["short", "verdict", "reason"],
            },
        },
        "notes": {"type": "string"},
    },
    "required": ["commits", "notes"],
}


def explain_commits(plan, scope):
    """Return the level 1+ commits that the auto or blocked pull request will carry."""
    if scope == "auto":
        commits = plan["commits"][:plan["auto"]["count"]]
    else:
        blocker = plan.get("blocker")
        commits = [commit for commit in plan["commits"] if blocker and commit["sha"] == blocker["sha"]]
    return [commit for commit in commits if commit["level"] >= 1]


def explain_prompt(scope, directory, count):
    pull_request = "automatic sync pull request" if scope == "auto" else "blocked sync pull request, which a human must review"
    return f"""You are reviewing the {pull_request} that merges upstream zed-industries/zed commits into the riso fork of Zed.
The pipeline has already classified every commit with deterministic rules and Jev. Those levels are final:
your verdicts are advisory, are shown next to them, and never change the level or whether anything is merged.

Read:
- {directory}/commits.json: the {count} commit(s) at level 1 or higher in this pull request, with the deterministic reasons for each level.
- {directory}/<short sha>.patch: the full upstream change of each of those commits.
- riso/adr/*.md and riso/hooks.toml: the fork's customizations, the upstream code they rely on, and their contracts.
- Any other repository file, to see the current fork code.

Return JSON matching the provided schema:
- "commits": one entry for every commit in commits.json, using its "short" value, with
  "verdict" = "safe" (the customization keeps working unchanged), "review" (probably fine, but a human should check
  something specific) or "risky" (the customization will likely break or need code changes), and
  "reason" = one Vietnamese sentence naming the concrete file, symbol or contract behind the verdict.
- "notes": Vietnamese Markdown, at most about 400 words, with no title: what the upstream changes do, how they affect
  the customizations, and a bullet list of concrete things the reviewer should verify.
"""


def write_explain_context(root, plan, scope, directory):
    """Write patches, commit summaries and the prompt; return False when nothing needs explaining."""
    commits = explain_commits(plan, scope)
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    if not commits:
        return False
    for commit in commits:
        (directory / f"{commit['short']}.patch").write_text(git(root, "show", "--stat", "--patch", commit["sha"]))
    summary = [{name: commit[name] for name in ("sha", "short", "subject", "level", "adrs", "reasons")} for commit in commits]
    (directory / "commits.json").write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n")
    (directory / "prompt.md").write_text(explain_prompt(scope, directory, len(commits)))
    (directory / "schema.json").write_text(json.dumps(REVIEW_SCHEMA))
    return True


def parse_json(text):
    text = text.strip()
    fenced = re.fullmatch(r"```(?:json)?\s*(.*?)\s*```", text, re.DOTALL)
    try:
        return json.loads(fenced.group(1) if fenced else text)
    except ValueError:
        return None


def parse_review(text):
    """Read Claude's review, from either the CLI's JSON envelope or the bare object, as notes plus verdicts by short SHA.

    Claude's output is untrusted: anything malformed is dropped instead of failing the sync.
    """
    empty = {"notes": "", "verdicts": {}}
    data = parse_json(text)
    if isinstance(data, dict) and data.get("type") == "result":
        if data.get("is_error"):
            return empty
        structured = data.get("structured_output")
        data = structured if structured is not None else parse_json(str(data.get("result", ""))) or data.get("result")
    if isinstance(data, str):
        return {"notes": data.strip(), "verdicts": {}}
    if not isinstance(data, dict):
        return {"notes": text.strip(), "verdicts": {}} if data is None and text.strip() else empty
    verdicts = {}
    for item in data.get("commits") if isinstance(data.get("commits"), list) else []:
        if (isinstance(item, dict) and isinstance(item.get("short"), str) and item.get("verdict") in VERDICTS
                and isinstance(item.get("reason"), str)):
            verdicts[item["short"].strip()[:7]] = {"verdict": item["verdict"], "reason": item["reason"].strip()}
    notes = data.get("notes")
    return {"notes": notes.strip() if isinstance(notes, str) else "", "verdicts": verdicts}


def load_review(path):
    """Load an optional review file; a missing file means the explain job was skipped or Claude failed."""
    if not path or not Path(path).is_file():
        return {"notes": "", "verdicts": {}}
    return parse_review(Path(path).read_text())


def conflicted_paths(root):
    return [path for path in git(root, "diff", "--name-only", "--diff-filter=U").splitlines() if path]


def resolve_prompt(target, directory, conflicts):
    files = "\n".join(f"- {path}" for path in conflicts)
    return f"""You are resolving merge conflicts in the riso fork of Zed, a customized fork that regularly merges upstream zed-industries/zed.
A merge of upstream commit {target} is in progress and these files contain conflict markers:
{files}

Read-only context:
- {directory}/upstream.patch: what the upstream commit changes in these files.
- {directory}/fork.diff: what the fork changed in these files relative to the merge base.
- riso/adr/*.md: the ADRs describing every fork customization (intent, owns, patches, hooks, relies_on, contracts).
- riso/hooks.toml: anchored insertions that are re-applied automatically after you finish; keep their anchors intact.

Rules:
- Edit only the files listed above, and remove every conflict marker from them.
- Keep the upstream change and preserve the customization the ADRs describe. If both cannot be kept, keep upstream and say which customization needs follow-up work.
- Do not edit Cargo.lock, README.md, ADRs or any other file; they are handled separately.
- A human reviews your result before anything is merged.

When you are done, print a review summary in Vietnamese Markdown with no title: for each file, how you resolved it and what the reviewer must verify. Print only that summary.
"""


def write_resolve_context(root, target, directory):
    """Write the upstream and fork diffs plus the prompt; return False when only Cargo.lock conflicts."""
    conflicts = [path for path in conflicted_paths(root) if path != "Cargo.lock"]
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    if not conflicts:
        return False
    base = git(root, "merge-base", "HEAD", target).strip()
    (directory / "upstream.patch").write_text(git(root, "diff", f"{target}^", target, "--", *conflicts))
    (directory / "fork.diff").write_text(git(root, "diff", base, "HEAD", "--", *conflicts))
    (directory / "conflicts.txt").write_text("\n".join(conflicts) + "\n")
    (directory / "prompt.md").write_text(resolve_prompt(target, directory, conflicts))
    return True


def stage_resolution(root, configuration):
    """Verify Claude's edits, regenerate Cargo.lock, re-apply hooks, add the review marker and commit the merge."""
    root = Path(root)
    try:
        git(root, "rev-parse", "-q", "--verify", "MERGE_HEAD")
    except RuntimeError as error:
        raise RuntimeError("No merge is in progress") from error
    unmerged = conflicted_paths(root)
    staged = []
    for path in unmerged:
        if path == "Cargo.lock":
            continue
        if any(line.startswith(CONFLICT_MARKERS) for line in (root / path).read_bytes().splitlines()):
            raise RuntimeError(f"Conflict marker remains in {path}")
        staged.append(path)
    if "Cargo.lock" in unmerged:
        git(root, "checkout", "--theirs", "--", "Cargo.lock")
        result = subprocess.run(["cargo", "update", "--workspace"], cwd=root, capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(f"cargo update --workspace: {result.stderr.strip()}")
        staged.append("Cargo.lock")
    hooks = load_hooks(root)
    for path in sorted({hook.file for hook in hooks}):
        text = (root / path).read_text()
        applied = apply_hooks_to_text(text, [hook for hook in hooks if hook.file == path])
        if applied != text:
            (root / path).write_text(applied)
            staged.append(path)
    marker = configuration.policy.review_marker_lines
    readme = root / "README.md"
    text = readme.read_text()
    if marker and text.splitlines()[:len(marker)] != marker:
        readme.write_text("\n".join(marker) + "\n" + text)
    staged.append("README.md")
    git(root, "add", "--", *dict.fromkeys(staged))
    leftovers = [line for line in git(root, "status", "--porcelain").splitlines()
                 if line.startswith("??") or line[0] == "U" or line[1] != " "]
    if leftovers:
        raise RuntimeError("Files changed outside the conflicted paths: " + ", ".join(line[3:] for line in leftovers))
    git(root, "commit", "--no-edit")
    return 0


def command(arguments):
    try:
        root = repository_root()
        if arguments.command == "claude-explain-context":
            plan = json.loads(Path(arguments.plan).read_text())
            if not write_explain_context(root, plan, arguments.scope, arguments.out):
                print(f"No level 1+ commits in the {arguments.scope} pull request; nothing to explain")
            return 0
        if arguments.command == "claude-resolve-context":
            if not write_resolve_context(root, arguments.target, arguments.out):
                print("Only Cargo.lock conflicts; Claude is not needed")
            return 0
        return stage_resolution(root, load_config(root))
    except (RuntimeError, ValueError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 1


def register(subparsers):
    """Register the Claude context and staging commands."""
    explain = subparsers.add_parser("claude-explain-context", help="Write Claude's context for explaining a sync pull request")
    explain.add_argument("--plan", required=True, help="Version 1 plan JSON file")
    explain.add_argument("--scope", required=True, choices=("auto", "blocked"))
    explain.add_argument("--out", required=True, help="Directory for the patches and prompt")
    resolution = subparsers.add_parser("claude-resolve-context", help="Write Claude's context for resolving blocker conflicts")
    resolution.add_argument("--target", required=True, help="Upstream commit being merged")
    resolution.add_argument("--out", required=True, help="Directory for the diffs and prompt")
    staging = subparsers.add_parser("claude-stage-resolution", help="Verify and commit Claude's conflict resolution")
    for parser in (explain, resolution, staging):
        parser.set_defaults(func=command)
