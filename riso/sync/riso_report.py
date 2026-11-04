"""Vietnamese synchronization reports without upstream PR backlinks."""

import html
import json
import re

BODY_LIMIT = 65000
EXPLANATION_LIMIT = 6000


def sanitize(value, title=False):
    value = str(value)
    if title:
        value = re.sub(r"\s*\(#\d+\)\s*$", "", value)
    value = re.sub(r"https?://github\.com/[^\s/]+/[^\s/]+/pull/\d+[^\s]*", "[PR upstream]", value)
    value = re.sub(r"\b[\w.-]+/[\w.-]+#(\d+)", r"PR \1", value)
    value = html.escape(value, quote=False).replace("@", "@\u200b").replace("|", "\\|")
    value = re.sub(r"([\[\]`])", r"\\\1", value)
    return " ".join(value.split())


def footer(plan, target):
    return f'<!-- riso-sync-plan head={plan["head"]} upstream={plan["upstream"]} target={target or "null"} -->'


def render(plan, config, blocked=False):
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
    sections = [header, "| Level | Số commit |\n| --- | --- |"]
    sections.extend(f'| L{level} | {sum(commit["level"] == level for commit in commits)} |' for level in range(5))
    sections.append("\n### Ảnh hưởng tới custom")
    grouped = {}
    for commit in commits:
        for reason in commit["reasons"]:
            identifier = reason["adr"] or ("Hooks" if reason["code"].startswith("hook_") else "Chung")
            grouped.setdefault(identifier, set()).add(f'{reason["code"]}: {", ".join(reason["paths"])}')
    sections.extend(f'- {sanitize(identifier)}: {sanitize("; ".join(sorted(values)))}' for identifier, values in sorted(grouped.items()))
    if not grouped:
        sections.append("Không phát hiện ảnh hưởng tới custom.")
    sections.extend(["\n### Commit từ L1 trở lên", "| Level | Commit | PR upstream | Tiêu đề | Lý do |", "| --- | --- | --- | --- | --- |"])
    for commit in commits:
        if commit["level"]:
            link = f'https://github.com/{plan["upstream_repo"]}/commit/{commit["sha"]}'
            number = f'`{commit["upstream_pr"]}`' if commit["upstream_pr"] else "—"
            reasons = "; ".join(reason["code"] + ": " + reason["message"] for reason in commit["reasons"])
            sections.append(f'| L{commit["level"]} | [{commit["short"]}]({link}) | {number} | {sanitize(commit["subject"], title=True)} | {sanitize(reasons)} |')
    sections.extend([f'\n### Quyết định của Jev ({sanitize(plan["jev"]["model"])})',
                     "| ADR | Commit | Model | Quyết định | Câu trả lời |", "| --- | --- | --- | --- | --- |"])
    decisions = False
    for commit in commits:
        for decision in commit["jev"]:
            decisions = True
            sections.append(f'| {sanitize(decision["adr"])} | {commit["short"]} | {sanitize(decision["model"])} | L{decision["decision_level"]}: {sanitize(decision["notes"])} | {sanitize(json.dumps(decision["answers"], ensure_ascii=False))} |')
    if not decisions:
        sections.append("Không có quyết định của Jev trong đợt này.")
    sections.append("\n### Xử lý cơ học")
    mechanical = sorted({path for commit in commits for path in commit["conflicts"]["mechanical"]})
    sections.extend(f'- {sanitize(path)}' for path in mechanical)
    if not mechanical:
        sections.append("Không có xung đột cơ học.")
    if blocked:
        sections.append("\nCần người kiểm tra trước khi tiếp tục đồng bộ.")
        real = sorted({path for commit in commits for path in commit["conflicts"]["real"]})
        sections.extend(f'- Xung đột: {sanitize(path)}' for path in real)
    marker = footer(plan, target)
    zero = [commit for commit in commits if not commit["level"]]
    details = "\n<details><summary>" + str(len(zero)) + " commit L0</summary>\n\n" + "\n".join(
        f'- {commit["short"]}: {sanitize(commit["subject"], title=True)}' for commit in zero) + "\n\n</details>\n"
    main = "\n".join(sections)
    if len(main + details + marker) < 60000:
        return main + details + "\n" + marker + "\n"
    note = "\n\nĐã rút gọn nội dung; xem plan.json để biết đầy đủ.\n"
    budget = 59999 - len(note) - len(marker) - 2
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
    text = explanation.strip().replace("<!--", "&lt;!--").replace("@", "@\u200b")
    section = f"\n### {heading}\n\n_{note}_\n\n"
    footer_start = body.rfind("<!-- riso-sync-plan")
    main, footer = (body, "") if footer_start == -1 else (body[:footer_start], body[footer_start:])
    # GitHub rejects bodies over 65536 characters; keep a margin for the truncation note.
    budget = min(EXPLANATION_LIMIT, BODY_LIMIT - len(body) - len(section) - 32)
    if not text or budget <= 0:
        return body
    if len(text) > budget:
        text = text[:budget].rstrip() + "\n\n…(đã rút gọn)"
    return main.rstrip("\n") + "\n" + section + text + "\n\n" + footer


def render_auto_body(plan, config):
    return render(plan, config)


def render_blocked_body(plan, config):
    return render(plan, config, blocked=True)


def render_waiting_comment(plan, pull_request_number):
    target = plan["auto"]["target"]
    return (f'Đang chờ xử lý mục đồng bộ bị chặn `{sanitize(pull_request_number)}` trước khi tiếp tục.\n'
            f'Upstream hiện tại: `{sanitize(plan["upstream"][:7])}`.\n\n{footer(plan, target)}\n')
