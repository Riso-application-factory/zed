"""Shared configuration, repository operations, and path matching."""

from pathlib import Path
import re
import subprocess
import tomllib


class Config(dict):
    """Dictionary configuration with attribute access for section keys."""

    def __getattr__(self, name):
        try:
            return self[name]
        except KeyError as error:
            raise AttributeError(name) from error


def git(repository_root, *arguments):
    """Run git and return stdout, raising an error containing stderr on failure."""
    result = subprocess.run(
        ["git", "-C", str(repository_root), *map(str, arguments)],
        capture_output=True, text=True,
    )
    if result.returncode:
        raise RuntimeError(f"git {' '.join(map(str, arguments))}: {result.stderr.strip()}")
    return result.stdout


def repository_root():
    """Locate the repository containing the current directory."""
    return Path(git(Path.cwd(), "rev-parse", "--show-toplevel").strip())


def load_config(repository_root=None):
    """Load and validate riso/config.toml with mapping and attribute access."""
    root = Path(repository_root) if repository_root is not None else globals()["repository_root"]()
    with (root / "riso/config.toml").open("rb") as source:
        configuration = tomllib.load(source)
    required = {
        "branches": dict.fromkeys("product mirror upstream_url upstream_branch upstream_repo upstream_ref sync_auto_branch sync_blocked_prefix".split(), str),
        "policy": {"shadow": bool, "auto_merge_max_level": int, "review_marker_lines": list},
        "labels": dict.fromkeys("sync blocked level_prefix".split(), str),
        "jev": {**dict.fromkeys("endpoint model api_key_env".split(), str),
                **dict.fromkeys("max_state_characters timeout_seconds max_retries".split(), int),
                **dict.fromkeys("yes_threshold no_threshold min_confidence safe_impact_max safe_impact_min_confidence breaking_impact l1_escalate_threshold".split(), (int, float))},
    }
    for section, fields in required.items():
        if not isinstance(configuration.get(section), dict):
            raise ValueError(f"Missing or invalid config section: {section}")
        for name, expected in fields.items():
            value = configuration[section].get(name)
            if not isinstance(value, expected) or (isinstance(value, bool) and expected is not bool):
                raise ValueError(f"Missing or invalid config key: {section}.{name}")
    if not all(isinstance(line, str) for line in configuration["policy"]["review_marker_lines"]):
        raise ValueError("policy.review_marker_lines must contain strings")
    return Config({name: Config(value) if isinstance(value, dict) else value for name, value in configuration.items()})


def glob_matches(pattern, path):
    """Match a repo-relative POSIX path using literal, *, **, and ? semantics."""
    expression = []
    position = 0
    while position < len(pattern):
        character = pattern[position]
        if character == "*":
            if pattern[position:position + 2] == "**":
                expression.append(".*")
                position += 2
                continue
            expression.append("[^/]*")
        elif character == "?":
            expression.append("[^/]")
        else:
            expression.append(re.escape(character))
        position += 1
    return re.fullmatch("".join(expression), path) is not None
