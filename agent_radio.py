#!/usr/bin/env python3
"""Local-only agent radio.

Persistent, file-backed messages for multiple local coding agents sharing the
same machine. By default state lives under `<git-root>/.git/.agent-radio/`
(never committed, never pushed); set AGENT_RADIO_DIR to use any directory and
run outside git worktrees entirely.

Environment:
    AGENT_RADIO_DIR    store directory (default: <git-root>/.git/.agent-radio)
    AGENT_RADIO_AGENT  default identity for --as/--from
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable


KINDS = {
    "FYI",
    "ASK",
    "REVIEW_REQUEST",
    "RISK",
    "BLOCKED",
    "HANDOFF",
    "ACK",
    "DONE",
    "DECLINE",
    "FAILURE",
}
REQUEST_KINDS = {"ASK", "REVIEW_REQUEST", "HANDOFF", "BLOCKED", "RISK"}
NAME_RE = re.compile(r"^[A-Za-z0-9._-]+$")
SECRET_PATTERNS = [
    re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b"),
    re.compile(r"\bgithub_pat_[A-Za-z0-9_]{40,}\b"),
    re.compile(r"\bsk-ant-[A-Za-z0-9_-]{40,}\b"),
    re.compile(r"\bsk-(?:proj-)?[A-Za-z0-9_-]{40,}\b"),
    re.compile(r"\bAIza[0-9A-Za-z_-]{20,}\b"),
    re.compile(r"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s:/@]+:[^\s/@]+@"),
    re.compile(
        r"(?i)(api[_-]?key|secret|token|password|passwd)\s*[:=]\s*['\"]?[A-Za-z0-9_./+=-]{24,}"
    ),
]


@dataclass
class Store:
    root: Path
    messages: Path
    lock: Path
    seen_dir: Path
    views_dir: Path
    notify_dir: Path


def git_root() -> Path:
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        raise SystemExit("agent-radio: run inside a git worktree")
    return Path(out.strip())


def current_branch() -> str | None:
    try:
        out = subprocess.check_output(
            ["git", "branch", "--show-current"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        return None
    return out or None


def store_root() -> Path:
    env = os.environ.get("AGENT_RADIO_DIR")
    if env:
        return Path(env).expanduser()
    return git_root() / ".git" / ".agent-radio"


def store() -> Store:
    root = store_root()
    return Store(
        root=root,
        messages=root / "messages.jsonl",
        lock=root / "lock",
        seen_dir=root / "seen",
        views_dir=root / "views",
        notify_dir=root / "notify",
    )


@contextmanager
def locked(s: Store):
    s.root.mkdir(parents=True, exist_ok=True)
    with s.lock.open("a+") as fh:
        fcntl.flock(fh.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(fh.fileno(), fcntl.LOCK_UN)


def validate_name(name: str) -> str:
    if not NAME_RE.match(name):
        raise SystemExit(
            f"agent-radio: invalid agent name {name!r}; use letters, digits, '.', '_' or '-'"
        )
    return name


def detect_secret(text: str) -> str | None:
    for pattern in SECRET_PATTERNS:
        if pattern.search(text):
            return pattern.pattern
    return None


def require_no_secret(text: str) -> None:
    matched = detect_secret(text)
    if matched:
        raise SystemExit(
            "agent-radio: message looks like it contains a secret. "
            "Do not send tokens, credentials, connection strings, or raw env output."
        )


def agent_from_env(explicit: str | None) -> str:
    name = explicit or os.environ.get("AGENT_RADIO_AGENT")
    if not name:
        raise SystemExit("agent-radio: pass --as/--from or set AGENT_RADIO_AGENT")
    return validate_name(name)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")


def gen_id(ts: str, sender: str, to: str, body: str) -> str:
    seed = f"{ts}\0{sender}\0{to}\0{body}\0{time.time_ns()}".encode()
    return hashlib.sha256(seed).hexdigest()[:16]


def load_messages(s: Store) -> list[dict[str, Any]]:
    if not s.messages.exists():
        return []
    out: list[dict[str, Any]] = []
    for line in s.messages.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(msg, dict) and isinstance(msg.get("id"), str):
            out.append(msg)
    return out


def append_message(s: Store, msg: dict[str, Any]) -> None:
    s.root.mkdir(parents=True, exist_ok=True)
    with s.messages.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(msg, ensure_ascii=False, sort_keys=True))
        fh.write("\n")


def notify_path(s: Store, agent: str) -> Path:
    return s.notify_dir / f"{agent}.flag"


def known_agents(messages: Iterable[dict[str, Any]]) -> set[str]:
    agents: set[str] = set()
    for msg in messages:
        sender = msg.get("from")
        to = msg.get("to")
        if isinstance(sender, str) and sender:
            agents.add(sender)
        if isinstance(to, str) and to and to != "all":
            agents.add(to)
    return agents


def set_notify_flags(s: Store, msg: dict[str, Any], messages: list[dict[str, Any]]) -> None:
    s.notify_dir.mkdir(parents=True, exist_ok=True)
    to = msg.get("to")
    recipients: set[str]
    if to == "all":
        recipients = known_agents(messages)
        recipients.discard(str(msg.get("from") or ""))
    elif isinstance(to, str) and to:
        recipients = {to}
    else:
        recipients = set()
    for agent in recipients:
        notify_path(s, agent).write_text(str(msg.get("id", "")), encoding="utf-8")


def clear_notify_if_caught_up(s: Store, agent: str, unread: list[dict[str, Any]]) -> None:
    if unread:
        return
    path = notify_path(s, agent)
    if path.exists():
        path.unlink()


def seen_path(s: Store, agent: str) -> Path:
    return s.seen_dir / f"{agent}.json"


def view_path(s: Store, agent: str) -> Path:
    return s.views_dir / f"{agent}.json"


def load_seen(s: Store, agent: str) -> set[str]:
    path = seen_path(s, agent)
    if not path.exists():
        return set()
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return set()
    return set(data.get("seen", []))


def save_seen(s: Store, agent: str, seen: set[str]) -> None:
    s.seen_dir.mkdir(parents=True, exist_ok=True)
    path = seen_path(s, agent)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps({"seen": sorted(seen)}, indent=2), encoding="utf-8")
    tmp.replace(path)


def save_view(s: Store, agent: str, ids: list[str]) -> None:
    s.views_dir.mkdir(parents=True, exist_ok=True)
    path = view_path(s, agent)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps({"ids": ids}, indent=2), encoding="utf-8")
    tmp.replace(path)


def load_view(s: Store, agent: str) -> list[str]:
    path = view_path(s, agent)
    if not path.exists():
        raise SystemExit("agent-radio: no last view; run inbox or history first")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        raise SystemExit("agent-radio: last view is corrupt; run inbox or history again")
    return list(data.get("ids", []))


def addressed_to(msg: dict[str, Any], agent: str) -> bool:
    return msg.get("to") == agent or (msg.get("to") == "all" and msg.get("from") != agent)


def unread_for(s: Store, agent: str) -> list[dict[str, Any]]:
    seen = load_seen(s, agent)
    return [m for m in load_messages(s) if addressed_to(m, agent) and m.get("id") not in seen]


def sanitize(text: Any) -> str:
    s = str(text or "")
    return " ".join(s.replace("\t", " ").replace("\r", " ").replace("\n", " ").split())


def short(text: Any, limit: int = 140) -> str:
    s = sanitize(text)
    return s if len(s) <= limit else s[: limit - 1] + "…"


def render(messages: Iterable[dict[str, Any]]) -> list[str]:
    lines: list[str] = []
    for idx, msg in enumerate(messages, start=1):
        branch = f" · {msg.get('branch')}" if msg.get("branch") else ""
        reply = f" · re {msg.get('reply_to')}" if msg.get("reply_to") else ""
        priority = f" · {msg.get('priority')}" if msg.get("priority") else ""
        lines.append(
            f"{idx:>2}. {msg.get('ts')} {msg.get('from')} -> {msg.get('to')} "
            f"{msg.get('kind')}{priority}{branch}{reply} #{msg.get('id')}"
        )
        focus = msg.get("focus") or []
        risk = msg.get("risk")
        if focus:
            lines.append(f"    focus: {', '.join(map(str, focus))}")
        if risk:
            lines.append(f"    risk : {short(risk, 180)}")
        lines.append(f"    {short(msg.get('body'), 260)}")
    return lines


def find_by_view_number(s: Store, agent: str, number: int) -> dict[str, Any]:
    ids = load_view(s, agent)
    if number < 1 or number > len(ids):
        raise SystemExit(f"agent-radio: no message #{number} in last view")
    wanted = ids[number - 1]
    for msg in load_messages(s):
        if msg.get("id") == wanted:
            return msg
    raise SystemExit(f"agent-radio: message #{number} is no longer available")


def make_message(
    sender: str,
    to: str,
    kind: str,
    body: str,
    branch: str | None,
    focus: list[str] | None = None,
    risk: str | None = None,
    priority: str | None = None,
    reply_to: str | None = None,
    thread_id: str | None = None,
) -> dict[str, Any]:
    validate_name(sender)
    validate_name(to)
    kind = kind.upper()
    if kind not in KINDS:
        raise SystemExit(f"agent-radio: invalid kind {kind}; use one of {', '.join(sorted(KINDS))}")
    body = body.strip()
    if not body:
        raise SystemExit("agent-radio: empty body")
    require_no_secret("\n".join([body, risk or "", "\n".join(focus or [])]))
    ts = utc_now()
    msg_id = gen_id(ts, sender, to, body)
    msg: dict[str, Any] = {
        "version": 1,
        "id": msg_id,
        "ts": ts,
        "from": sender,
        "to": to,
        "kind": kind,
        "body": body,
    }
    if branch:
        msg["branch"] = branch
    if focus:
        msg["focus"] = focus
    if risk:
        msg["risk"] = risk
    if priority:
        msg["priority"] = priority.lower()
    if reply_to:
        msg["reply_to"] = reply_to
    if thread_id:
        msg["thread_id"] = thread_id
    elif reply_to:
        msg["thread_id"] = reply_to
    return msg


def cmd_send(args: argparse.Namespace) -> None:
    s = store()
    sender = agent_from_env(args.sender)
    branch = args.branch if args.branch is not None else current_branch()
    msg = make_message(
        sender=sender,
        to=validate_name(args.to),
        kind=args.kind,
        body=args.body,
        branch=branch,
        focus=args.focus,
        risk=args.risk,
        priority=args.priority,
    )
    with locked(s):
        messages = load_messages(s)
        append_message(s, msg)
        set_notify_flags(s, msg, [*messages, msg])
    print(f"sent {msg['kind']} {msg['from']} -> {msg['to']} #{msg['id']}")


def cmd_inbox(args: argparse.Namespace) -> None:
    s = store()
    agent = agent_from_env(args.as_agent)
    with locked(s):
        seen = load_seen(s, agent)
        messages = unread_for(s, agent)
        save_view(s, agent, [m["id"] for m in messages])
        if not args.peek:
            seen.update(m["id"] for m in messages)
            save_seen(s, agent, seen)
            clear_notify_if_caught_up(s, agent, unread_for(s, agent))
    if not messages:
        print(f"inbox for {agent}: empty")
        return
    print("\n".join(render(messages)))


def cmd_history(args: argparse.Namespace) -> None:
    s = store()
    agent = agent_from_env(args.as_agent) if args.as_agent else None
    messages = load_messages(s)
    if args.with_agent:
        messages = [m for m in messages if args.with_agent in {m.get("from"), m.get("to")}]
    if args.branch:
        messages = [m for m in messages if m.get("branch") == args.branch]
    messages = messages[-args.limit :]
    if agent:
        with locked(s):
            save_view(s, agent, [m["id"] for m in messages])
    if not messages:
        print("history: empty")
        return
    print("\n".join(render(messages)))


def reply_target(original: dict[str, Any], me: str) -> str:
    if original.get("from") == me:
        return str(original.get("to"))
    return str(original.get("from"))


def cmd_reply_kind(args: argparse.Namespace, kind: str) -> None:
    s = store()
    me = agent_from_env(args.as_agent)
    with locked(s):
        original = find_by_view_number(s, me, args.number)
        body = args.body.strip() or kind.lower()
        msg = make_message(
            sender=me,
            to=validate_name(reply_target(original, me)),
            kind=kind,
            body=body,
            branch=original.get("branch"),
            reply_to=original.get("id"),
            thread_id=original.get("thread_id") or original.get("id"),
        )
        messages = load_messages(s)
        append_message(s, msg)
        set_notify_flags(s, msg, [*messages, msg])
    print(f"sent {kind} re #{original['id']} -> {msg['to']} #{msg['id']}")


def cmd_team(_: argparse.Namespace) -> None:
    agents: dict[str, str] = {}
    for msg in load_messages(store()):
        if msg.get("from"):
            agents[str(msg["from"])] = str(msg.get("ts", ""))
        if msg.get("to") and msg.get("to") != "all":
            agents.setdefault(str(msg["to"]), "")
    if not agents:
        print("team: empty")
        return
    for name, ts in sorted(agents.items()):
        print(f"{name}\t{ts}")


def cmd_status(args: argparse.Namespace) -> None:
    s = store()
    agent = agent_from_env(args.as_agent)
    with locked(s):
        unread = unread_for(s, agent)
        flagged = notify_path(s, agent).exists()
        if not unread:
            clear_notify_if_caught_up(s, agent, unread)
            flagged = False
    if args.quiet:
        raise SystemExit(0 if unread else 1)
    print(json.dumps({"agent": agent, "unread": len(unread), "flag": flagged}, sort_keys=True))


def cmd_wait(args: argparse.Namespace) -> None:
    s = store()
    agent = agent_from_env(args.as_agent)
    deadline = time.monotonic() + args.timeout
    while True:
        with locked(s):
            unread = unread_for(s, agent)
            if unread:
                save_view(s, agent, [m["id"] for m in unread])
                print("\n".join(render(unread)))
                return
            clear_notify_if_caught_up(s, agent, unread)
        if time.monotonic() >= deadline:
            raise SystemExit(1)
        time.sleep(args.interval)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Local-only persistent agent messages")
    sub = parser.add_subparsers(dest="cmd", required=True)

    send = sub.add_parser("send", help="send a typed message")
    send.add_argument("--from", dest="sender")
    send.add_argument("--to", required=True)
    send.add_argument("--kind", default="ASK")
    send.add_argument("--body", required=True)
    send.add_argument("--branch", help="default: current git branch; pass '' to omit")
    send.add_argument("--focus", action="append", default=[])
    send.add_argument("--risk")
    send.add_argument("--priority", choices=["low", "normal", "high", "urgent"])
    send.set_defaults(func=cmd_send)

    inbox = sub.add_parser("inbox", help="show unread messages for an agent")
    inbox.add_argument("--as", dest="as_agent")
    inbox.add_argument("--peek", action="store_true", help="do not mark messages read")
    inbox.set_defaults(func=cmd_inbox)

    history = sub.add_parser("history", help="show recent messages")
    history.add_argument("--as", dest="as_agent", help="save numbering for replies")
    history.add_argument("--limit", type=int, default=30)
    history.add_argument("--with", dest="with_agent")
    history.add_argument("--branch")
    history.set_defaults(func=cmd_history)

    for name, kind in [("reply", "ACK"), ("ack", "ACK"), ("done", "DONE"), ("decline", "DECLINE"), ("failure", "FAILURE")]:
        p = sub.add_parser(name, help=f"reply to a numbered message with {kind}")
        p.add_argument("number", type=int)
        p.add_argument("--as", dest="as_agent")
        p.add_argument("--body", default="")
        p.set_defaults(func=lambda args, k=kind: cmd_reply_kind(args, k))

    team = sub.add_parser("team", help="list known agents")
    team.set_defaults(func=cmd_team)

    status = sub.add_parser("status", help="show unread count and notify flag")
    status.add_argument("--as", dest="as_agent")
    status.add_argument("--quiet", action="store_true", help="exit 0 when unread exists, 1 otherwise")
    status.set_defaults(func=cmd_status)

    wait = sub.add_parser("wait", help="wait until unread messages arrive")
    wait.add_argument("--as", dest="as_agent")
    wait.add_argument("--timeout", type=float, default=300.0)
    wait.add_argument("--interval", type=float, default=2.0)
    wait.set_defaults(func=cmd_wait)
    return parser


def main() -> None:
    args = build_parser().parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
