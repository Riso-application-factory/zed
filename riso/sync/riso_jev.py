"""Filtered System One requests and conservative decision rules."""

import json
import math
import os
import time
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


class JevError(RuntimeError):
    pass


class StateTooLarge(JevError):
    pass


def section(config):
    return config.get("jev", config)


def questions(adr, full=True):
    def noul(instructions, yes, no):
        return {"type": "noul", "instructions": instructions,
                "criteria": {"true": yes, "false": no}}
    result = {"relied_behavior_changed": noul(
        "Does `upstream_change.diff` change the signature or the behavior of any item listed in `customization.relies_on`?",
        "A listed item's signature or behavior changes.", "No listed item's signature or behavior changes.")}
    if not full:
        return result
    result["impact"] = {"type": "score", "instructions": "How does upstream_change.diff affect customization?",
                        "criteria": ["Unrelated to the customization.",
                                     "Touches nearby code but nothing listed in `customization.relies_on`.",
                                     "Changes something in `customization.relies_on`, but the customization should keep working.",
                                     "The customization will likely break or need code changes."]}
    for index, contract in enumerate(adr.contracts):
        result[f"contract_{index}_broken"] = noul(
            f"After the change in `upstream_change.diff`, could `customization.contracts[{index}]` stop passing?",
            "The contract could stop passing because of this change.", "The contract should keep passing.")
    if adr.obsolete_if:
        result["upstream_overlap"] = noul(
            "Does `upstream_change.diff` add functionality matching `customization.obsolete_if`?",
            "Matching functionality is added.", "Matching functionality is not added.")
    result["restructure"] = noul(
        "Does `upstream_change.diff` move, rename, split, or rewrite the code that `customization.relies_on` refers to?",
        "Referenced code is moved, renamed, split, or rewritten.", "Referenced code retains its structure.")
    return result


def build_state(adr, title, files, diff, custom_code, config):
    state = {"customization": {name: getattr(adr, name) for name in
                               ("id", "intent", "relies_on", "contracts", "obsolete_if")},
             "upstream_change": {"title": title, "files": files, "diff": diff},
             "custom_code": custom_code}
    if len(json.dumps(state)) > section(config)["max_state_characters"]:
        raise StateTooLarge("jev_state_too_large")
    return state


def combine(answers, config):
    configuration = section(config)
    impact = answers.get("impact", {})
    values = {name: answer["noul"] for name, answer in answers.items() if "noul" in answer}
    if (any(configuration["no_threshold"] <= value <= configuration["yes_threshold"] for value in values.values())
            or impact.get("confidence", 0) < configuration["min_confidence"]):
        return 3, "jev_uncertain"
    if any(values.get(name, 0) > configuration["yes_threshold"] for name in ("restructure", "upstream_overlap")):
        return 4, "jev_restructure_or_overlap"
    if (impact.get("score", 3) >= configuration["breaking_impact"]
            or any(value > configuration["yes_threshold"] for name, value in values.items()
                   if name.startswith("contract_") and name.endswith("_broken"))):
        return 3, "jev_breaking"
    if (impact.get("score", 3) < configuration["safe_impact_max"]
            and impact.get("confidence", 0) >= configuration["safe_impact_min_confidence"]
            and all(value < configuration["no_threshold"] for value in values.values())):
        return 2, "safe"
    return 3, "jev_review_required"


class JevClient:
    def __init__(self, config, opener=None, sleeper=None):
        self.config = section(config)
        self.opener = opener or urlopen
        self.sleeper = sleeper or time.sleep

    def request(self, state, question_set):
        configuration = self.config
        if len(json.dumps(state)) > configuration["max_state_characters"]:
            raise StateTooLarge("jev_state_too_large")
        key = os.environ.get(configuration["api_key_env"])
        if not key:
            raise JevError("jev_unavailable: API key is missing")
        request = Request(configuration["endpoint"], data=json.dumps({
            "model": configuration["model"], "state": state, "questions": question_set}, allow_nan=False).encode(),
            headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"}, method="POST")
        for attempt in range(configuration["max_retries"] + 1):
            try:
                with self.opener(request, timeout=configuration["timeout_seconds"]) as response:
                    result = json.load(response)
                if not isinstance(result, dict) or not isinstance(result.get("model"), str) or not result["model"]:
                    raise ValueError("Missing response model")
                for name, question in question_set.items():
                    answer = result["answers"][name]
                    fields = ("noul",) if question["type"] == "noul" else ("score", "confidence")
                    for field in fields:
                        value = answer[field]
                        maximum = len(question.get("criteria", [])) - 1 if field == "score" else 1
                        if isinstance(value, bool) or not isinstance(value, (float, int)) or not math.isfinite(value) or not 0 <= value <= maximum:
                            raise ValueError(f"Invalid {name}.{field}")
                return result
            except HTTPError as error:
                if error.code not in (429, 529) or attempt == configuration["max_retries"]:
                    raise JevError(f"Jev HTTP {error.code}: {error.reason}") from error
                retry_after = error.headers.get("retry-after") if error.headers else None
                delay = 2 ** attempt
                if retry_after:
                    try:
                        delay = max(delay, float(retry_after))
                    except ValueError:
                        try:
                            delay = max(delay, (parsedate_to_datetime(retry_after) - datetime.now(timezone.utc)).total_seconds())
                        except (ValueError, TypeError) as parse_error:
                            raise JevError("Invalid Retry-After header") from parse_error
                self.sleeper(delay)
            except (URLError, OSError, ValueError, KeyError, TypeError) as error:
                raise JevError(f"Jev request failed: {error}") from error


def evaluate(adr, state, baseline, config, client=None):
    client = client or JevClient(config)
    records = []
    response = client.request(state, questions(adr, full=baseline != 1))
    if baseline == 1:
        escalated = response["answers"]["relied_behavior_changed"]["noul"] > section(config)["l1_escalate_threshold"]
        records.append({"adr": adr.id, "model": response["model"], "answers": response["answers"],
                        "decision_level": 2 if escalated else 1, "notes": "jev_l1_escalation" if escalated else "safe"})
        if not escalated:
            return 1, "safe", records
        response = client.request(state, questions(adr))
    level, notes = combine(response["answers"], config)
    records.append({"adr": adr.id, "model": response["model"], "answers": response["answers"],
                    "decision_level": level, "notes": notes})
    return max(baseline, level), notes, records
