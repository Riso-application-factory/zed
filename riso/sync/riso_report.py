"""Vietnamese synchronization reports without upstream PR backlinks."""

import html
import json
import re

BODY_LIMIT = 65000
EXPLANATION_LIMIT = 6000
REPORT_LIMIT = 60000
CLAUDE_REASON_LIMIT = 240

RULE_LABELS = {
    "patched_file": "file custom đã patch",
    "watch": "file đang theo dõi",
    "surface_removed": "xoá hoặc đổi tên bề mặt custom",
    "hook_file": "file có hook",
    "hook_anchor": "mất anchor của hook",
    "dependency_crate": "crate mà custom phụ thuộc",
    "symbol_definition": "đổi symbol mà custom dùng",
    "mechanical_conflict": "xung đột cơ học",
    "conflict": "xung đột thật",
    "review_policy": "ADR yêu cầu người review",
}
JEV_LABELS = {
    "safe": "an toàn",
    "jev_l1_escalation": "nâng lên L2",
    "jev_uncertain": "không chắc chắn",
    "jev_breaking": "có thể hỏng",
    "jev_review_required": "cần review",
    "jev_restructure_or_overlap": "tái cấu trúc hoặc trùng upstream",
}
JEV_FAILURES = ("jev_unavailable", "jev_state_too_large")
CLAUDE_LABELS = {"safe": "✅ an toàn", "review": "👀 cần review", "risky": "⚠️ có thể hỏng"}


def sanitize(value, title=False):
    value = str(value)
    if title:
        value = re.sub(r"\s*\(#\d+\)\s*$", "", value)
    value = re.sub(r"https?://github\.com/[^\s/]+/[^\s/]+/pull/\d+[^\s]*", "[PR upstream]", value)
    value = re.sub(r"\b[\w.-]+/[\w.-]+#(\d+)", r"PR \1", value)
    value = html.escape(value, quote=False).replace("@", "@​").replace("|", "\\|")
    value = re.sub(r"([\[\]`])", r"\\\1", value)
    return " ".join(value.split())


def footer(plan, target):
    return f'<!-- riso-sync-plan head={plan["head"]} upstream={plan["upstream"]} target={target or "null"} -->'


def rule_cell(commit):
    grouped = {}
    for reason in commit["reasons"]:
        if not reason["code"].startswith("jev_"):
            grouped.setdefault(reason["code"], set()).update(reason["paths"])
    parts = []
    for code, paths in grouped.items():
        where = ""
        if paths:
            first, *rest = sorted(paths)
            where = f" {first}" + (f" (+{len(rest)})" if rest else "")
        parts.append(RULE_LABELS.get(code, code) + where)
    adrs = f' ({", ".join(commit["adrs"])})' if commit["adrs"] else ""
    return sanitize(f'L{commit["baseline_level"]}' + (": " + "; ".join(parts) if parts else "") + adrs)


def jev_cell(commit):
    latest = {}
    for decision in commit["jev"]:
        latest[decision["adr"]] = decision
    if latest:
        parts = []
        for adr, decision in latest.items():
            impact = decision["answers"].get("impact") if isinstance(decision["answers"], dict) else None
            detail = ""
            relied = decision["answers"].get("relied_behavior_changed") if isinstance(decision["answers"], dict) else None
            if (isinstance(impact, dict) and isinstance(impact.get("score"), (int, float))
                    and isinstance(impact.get("confidence"), (int, float))):
                detail = f' (impact {impact["score"]:.1f}, tin cậy {impact["confidence"]:.0%})'
            elif isinstance(relied, dict) and isinstance(relied.get("noul"), (int, float)):
                detail = f' (khả năng đổi hành vi custom dựa vào: {relied["noul"]:.0%})'
            prefix = f"{adr}: " if len(latest) > 1 else ""
            parts.append(f'{prefix}L{decision["decision_level"]} {JEV_LABELS.get(decision["notes"], decision["notes"])}{detail}')
        return sanitize("; ".join(parts))
    if any(reason["code"] in JEV_FAILURES for reason in commit["reasons"]):
        return "⚠️ không gọi được"
    if commit["baseline_level"] == 0:
        return "—"
    if commit["baseline_level"] >= 3:
        return "— (luật đã chặn)"
    return "— (không cần)"


def claude_cell(commit, verdicts):
    verdict = verdicts.get(commit["short"])
    if not verdict or verdict.get("verdict") not in CLAUDE_LABELS:
        return "—"
    reason = " ".join(str(verdict.get("reason", "")).split())
    if len(reason) > CLAUDE_REASON_LIMIT:
        reason = reason[:CLAUDE_REASON_LIMIT].rstrip() + "…"
    return CLAUDE_LABELS[verdict["verdict"]] + (f": {sanitize(reason)}" if reason else "")


def action_cell(commit):
    level = commit["level"]
    action = "⛔ chặn, cần người xử lý" if level >= 3 else "👀 cần review" if level == 2 else "✅ tự động"
    if commit["conflicts"]["mechanical"]:
        action += "; giải tự động: " + sanitize(", ".join(commit["conflicts"]["mechanical"]))
    if commit["conflicts"]["real"]:
        action += "; xung đột: " + sanitize(", ".join(commit["conflicts"]["real"]))
    return action


def commit_row(plan, commit, verdicts):
    link = f'https://github.com/{plan["upstream_repo"]}/commit/{commit["sha"]}'
    title = sanitize(commit["subject"], title=True) + (f' · `{commit["upstream_pr"]}`' if commit["upstream_pr"] else "")
    level = f'**L{commit["level"]}**' if commit["level"] >= 2 else f'L{commit["level"]}'
    return (f'| [{commit["short"]}]({link}) | {title} | {rule_cell(commit)} | {jev_cell(commit)} | '
            f'{claude_cell(commit, verdicts)} | {level} | {action_cell(commit)} |')


def render(plan, config, blocked=False, verdicts=None):
    verdicts = verdicts or {}
    blocker = plan.get("blocker")
    target = blocker["sha"] if blocked and blocker else plan["auto"]["target"]
    if blocked:
        commits = [commit for commit in plan["commits"] if blocker and commit["sha"] == blocker["sha"]]
    else:
        commits = plan["commits"][:plan["auto"]["count"]]
    maximum = max((commit["level"] for commit in commits), default=0)
    policy = config["policy"]
    mode = "❌" if blocked else "⏸ (shadow)" if policy["shadow"] else "✅" if plan["auto"]["auto_merge_eligible"] else "❌"
    header = f'## Sync upstream {plan["base"][:7]} → {(target or plan["head"])[:7]} · {len(commits)} commit · cao nhất L{maximum} · auto-merge {mode}\n'
    counts = "Số commit theo level: " + " · ".join(f'L{level}: {sum(commit["level"] == level for commit in commits)}' for level in range(5))
    table_head = ["\n### Các commit trong PR này",
                  "_Level cuối do luật và Jev quyết định; cột Claude chỉ để tham khảo, không ảnh hưởng tới level hay auto-merge._\n",
                  "| Commit | Tiêu đề | Luật | Jev | Claude (tham khảo) | Level | Xử lý |",
                  "| --- | --- | --- | --- | --- | --- | --- |"]
    rows = [(commit, commit_row(plan, commit, verdicts)) for commit in commits]
    tail = []
    if blocked:
        tail.append("\nCần người kiểm tra trước khi tiếp tục đồng bộ.")
        real = sorted({path for commit in commits for path in commit["conflicts"]["real"]})
        tail.extend(f'- Xung đột: {sanitize(path)}' for path in real)
    decisions = [(commit, decision) for commit in commits for decision in commit["jev"]]
    if decisions:
        tail.extend([f'\n<details><summary>Chi tiết câu trả lời của Jev ({sanitize(plan["jev"]["model"])})</summary>\n',
                     "| ADR | Commit | Model | Quyết định | Câu trả lời |", "| --- | --- | --- | --- | --- |"])
        tail.extend(f'| {sanitize(decision["adr"])} | {commit["short"]} | {sanitize(decision["model"])} | '
                    f'L{decision["decision_level"]}: {sanitize(decision["notes"])} | '
                    f'{sanitize(json.dumps(decision["answers"], ensure_ascii=False))} |'
                    for commit, decision in decisions)
        tail.append("\n</details>")
    marker = footer(plan, target)

    def assemble(table_rows):
        return "\n".join([header, counts, *table_head, *table_rows, *tail])

    main = assemble([row for _, row in rows])
    if len(main) + len(marker) + 2 >= REPORT_LIMIT:
        zero = sum(not commit["level"] for commit, _ in rows)
        collapsed = [row for commit, row in rows if commit["level"]]
        if zero:
            collapsed.append(f"| … | {zero} commit L0 đã được rút gọn; xem plan.json | L0 | — | — | L0 | ✅ tự động |")
        main = assemble(collapsed)
    if len(main) + len(marker) + 2 < REPORT_LIMIT:
        return main + "\n\n" + marker + "\n"
    note = "\n\nĐã rút gọn nội dung; xem plan.json để biết đầy đủ.\n"
    budget = REPORT_LIMIT - 1 - len(note) - len(marker) - 2
    kept = []
    length = 0
    for line in main.splitlines():
        if length + len(line) + 1 > budget:
            break
        kept.append(line)
        length += len(line) + 1
    return "\n".join(kept) + note + marker + "\n"


def append_explanation(body, explanation, heading="Giải thích của Claude",
                       note="Do Claude viết, chỉ để tham khảo; không ảnh hưởng tới level hay auto-merge."):
    """Insert untrusted Claude prose above the plan footer; it can neither mention users nor hide markup."""
    text = explanation.strip().replace("<!--", "&lt;!--").replace("@", "@​")
    section = f"\n### {heading}\n\n_{note}_\n\n"
    footer_start = body.rfind("<!-- riso-sync-plan")
    main, footer_text = (body, "") if footer_start == -1 else (body[:footer_start], body[footer_start:])
    # GitHub rejects bodies over 65536 characters; keep a margin for the truncation note.
    budget = min(EXPLANATION_LIMIT, BODY_LIMIT - len(body) - len(section) - 32)
    if not text or budget <= 0:
        return body
    if len(text) > budget:
        text = text[:budget].rstrip() + "\n\n…(đã rút gọn)"
    return main.rstrip("\n") + "\n" + section + text + "\n\n" + footer_text


def render_auto_body(plan, config, verdicts=None):
    return render(plan, config, verdicts=verdicts)


def render_blocked_body(plan, config, verdicts=None):
    return render(plan, config, blocked=True, verdicts=verdicts)


def render_waiting_comment(plan, pull_request_number):
    target = plan["auto"]["target"]
    return (f'Đang chờ xử lý mục đồng bộ bị chặn `{sanitize(pull_request_number)}` trước khi tiếp tục.\n'
            f'Upstream hiện tại: `{sanitize(plan["upstream"][:7])}`.\n\n{footer(plan, target)}\n')
