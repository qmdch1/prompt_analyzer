"""Import past turns from the Claude Code and Codex transcripts on this machine.

install.py runs this on install and on `--import`; hook.py starts it in the background at most once an hour
so turns missed while the server was down get filled in. It finds the transcript folders itself (this
home, plus the Windows home when running in WSL), reads only *.jsonl transcripts, and skips files that
haven't changed since the last run. The server drops turns it already has, so running it again is safe.
"""
import fcntl
import glob
import hashlib
import json
import os
import re
import time
import urllib.request
from pathlib import Path

STATE_DIR = Path.home() / ".prompt-analyzer"
CODEX_INTERNAL = "# Overview\n\nGenerate 0 to 3 hyperpersonalized suggestions"
# Text Claude Code writes as a "user" entry that nobody typed: reminders, command output, interrupt and image markers.
WRAPPERS = ("<system-reminder>", "<local-command", "<command-message>", "<bash-", "<task-notification", "<user-prompt-submit-hook>",
            "[Request interrupted", "[Image: original")


def transcript_files(homes):
    files = []
    for home in homes:
        files += glob.glob(str(home / ".claude" / "projects" / "*" / "*.jsonl"))  # not subagent files below
        files += glob.glob(str(home / ".codex" / "sessions" / "**" / "*.jsonl"), recursive=True)
        files += glob.glob(str(home / ".codex" / "archived_sessions" / "*.jsonl"))
    return sorted(set(files))


def read_jsonl(path):
    entries = []
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            try:
                entries.append(json.loads(line))
            except ValueError:
                pass
    return entries


def prompt_of(content):
    """What the user typed: the text blocks minus system reminders and wrappers the agent adds. A slash
    command ("<command-name>/goal</command-name> … <command-args>…") is a request too: "/goal …"."""
    parts = [content] if isinstance(content, str) else \
        [b.get("text", "") for b in content or [] if isinstance(b, dict) and b.get("type") == "text"]
    typed = []
    for part in parts:
        command = re.search(r"<command-name>(.*?)</command-name>", part, re.S)
        if command:
            args = re.search(r"<command-args>(.*?)</command-args>", part, re.S)
            typed.append(f"{command.group(1).strip()} {args.group(1).strip() if args else ''}".strip())
        elif not part.strip().startswith(WRAPPERS):
            typed.append(part)
    return "\n".join(typed).strip()


def is_human(e):
    if e.get("type") != "user" or e.get("isSidechain") or e.get("isMeta") or e.get("isCompactSummary") or e.get("isVisibleInTranscriptOnly"):
        return False  # a compaction summary continues the running turn rather than starting one
    content = (e.get("message") or {}).get("content")
    origin = e.get("origin")
    if origin is not None and origin.get("kind") != "human":
        return False
    if origin is None and isinstance(content, list) and any(b.get("type") == "tool_result" for b in content):
        return False
    return bool(prompt_of(content))


def claude_turns(entries, stale):
    """One turn per typed prompt. A turn without a final answer (interrupted) is folded into the next prompt,
    as the hooks record it. A trailing unfinished turn is kept only when the file is `stale` (no longer
    written); otherwise it may still be running and the hooks will record it."""
    turns, cur, carry = [], None, None

    def finish(at_end=False):
        nonlocal carry
        if cur is None or not cur["usage"]:
            return
        if not cur["done"] and not at_end:
            carry = cur
            return
        if not cur["done"] and not stale:
            return
        usage = cur["usage"].values()
        cached = sum(u.get("cache_read_input_tokens", 0) for u in usage)
        turns.append({
            "source": "claude", "external_id": cur["session"], "prompt": "\n\n".join(cur["prompts"]),
            "response": (list(cur["texts"].values()) or [""])[-1][:2000], "model": cur["model"],
            "input_tokens": sum(u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) for u in usage) + cached,
            "cached_tokens": cached, "output_tokens": sum(u.get("output_tokens", 0) for u in usage),
            "started_at": cur["started"], "completed_at": cur["last"],
        })

    for e in entries:
        if is_human(e):
            finish()
            prompt = prompt_of(e["message"]["content"])
            if carry:
                cur, carry = dict(carry, prompts=carry["prompts"] + [prompt], done=False), None
            else:
                cur = {"session": e.get("sessionId"), "started": e.get("timestamp"), "prompts": [prompt],
                       "usage": {}, "texts": {}, "model": None, "last": None, "done": False}
            continue
        m = e.get("message") or {}
        if cur is None or e.get("type") != "assistant" or e.get("isSidechain") or not m.get("usage") or m.get("model") == "<synthetic>":
            continue
        cur["usage"][m.get("id")] = m["usage"]  # streamed blocks repeat one message's usage
        cur["model"] = m.get("model") or cur["model"]
        cur["last"] = e.get("timestamp")
        cur["done"] = cur["done"] or m.get("stop_reason") not in (None, "tool_use")
        text = "".join(b.get("text", "") for b in m.get("content") or [] if b.get("type") == "text")
        if text:
            cur["texts"][m.get("id")] = cur["texts"].get(m.get("id"), "") + text
    finish(at_end=True)
    return [t for t in turns if t["external_id"] and t["started_at"]]


def codex_turns(entries, stale):
    """One turn per task (task_started → task_complete); tokens are the sum of its requests' usage.
    An aborted task is folded into the next one, as the hooks record it; an unfinished or aborted last task
    counts only when the file is `stale`."""
    meta = next((e.get("payload") or {} for e in entries if e.get("type") == "session_meta"), {})
    if isinstance(meta.get("source"), dict) and "subagent" in meta["source"]:
        return []  # a subagent's thread, not something the user typed (Claude subagents are left out too)
    thread = meta.get("id") or meta.get("session_id")
    turns, cur, carry, last_total, model = [], None, None, None, None

    def emit(c):
        prompt = "\n\n".join(p for p in c["prompts"] if p).strip()
        if not prompt or not c["requests"] or prompt.startswith(CODEX_INTERNAL):
            return
        turns.append({
            "source": "codex", "external_id": thread, "prompt": prompt, "response": c["response"][:2000],
            "model": c["model"] or model, "input_tokens": c["used"]["input_tokens"], "cached_tokens": c["used"]["cached_input_tokens"],
            "output_tokens": c["used"]["output_tokens"], "started_at": c["started"], "completed_at": c["done"],
        })

    for e in entries:
        p = e.get("payload") or {}
        kind = p.get("type")
        if e.get("type") == "turn_context" and p.get("model"):
            model = p["model"]
            if cur:
                cur["model"] = model
        if kind == "task_started":
            if carry:
                cur, carry = dict(carry, prompts=carry["prompts"] + [""], done=None), None
            else:
                cur = {"started": e.get("timestamp"), "used": {"input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0},
                       "requests": 0, "prompts": [""], "response": "", "model": model, "done": None}
        elif kind == "token_count" and (p.get("info") or {}).get("last_token_usage"):
            # one request's usage; a repeated token_count carries the same running total and is skipped
            total = p["info"].get("total_token_usage")
            if cur and total != last_total:
                for k in cur["used"]:
                    cur["used"][k] += p["info"]["last_token_usage"].get(k, 0)
                cur["requests"] += 1
            last_total = total
        elif cur is None:
            continue
        elif kind == "user_message" and not cur["prompts"][-1]:
            cur["prompts"][-1] = (p.get("message") or "").strip()
        elif kind == "item_completed" and (p.get("item") or {}).get("type") == "UserMessage" and not cur["prompts"][-1]:
            # the Codex app writes the typed prompt only as a UserMessage item
            cur["prompts"][-1] = "\n".join(b.get("text", "") for b in p["item"].get("content") or [] if b.get("type") == "text").strip()
        elif kind == "agent_message" and p.get("message"):
            cur["response"] = p["message"]
        elif kind == "task_complete":
            cur["response"] = p.get("last_agent_message") or cur["response"]
            cur["done"] = e.get("timestamp")
            emit(cur)
            cur = None
        elif kind == "turn_aborted":
            carry, cur = cur, None
    if stale:
        for leftover in (carry, cur):
            if leftover:
                emit(leftover)
    return turns if thread else []


def turns_in(path):
    entries = read_jsonl(path)
    stale = time.time() - os.stat(path).st_mtime > 1800  # untouched for 30 minutes: nothing is still running
    if any(e.get("type") == "session_meta" for e in entries):
        return codex_turns(entries, stale)
    if any(e.get("type") == "assistant" for e in entries):
        return claude_turns(entries, stale)
    return []


def run(api, homes):
    """Imports new turns; returns (conversations, added, skipped, updated), or None if another import is running."""
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    lock = open(STATE_DIR / "import.lock", "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        return None
    state_file = STATE_DIR / f"imported-{hashlib.sha1(api.encode()).hexdigest()[:8]}.json"
    try:
        state = json.loads(state_file.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        state = {}
    conversations = added = skipped = updated = 0
    for path in transcript_files(homes):
        stat = os.stat(path)
        signature = [stat.st_size, int(stat.st_mtime)]
        if state.get(path) == signature:
            continue
        turns = turns_in(path)
        if turns:
            req = urllib.request.Request(f"{api}/v1/import", json.dumps({"turns": turns}).encode(), {"content-type": "application/json"})
            with urllib.request.urlopen(req, timeout=120) as res:
                result = json.loads(res.read())
            conversations, added, skipped = conversations + 1, added + result["added"], skipped + result["skipped"]
            updated += result.get("updated", 0)
        state[path] = signature
        state_file.write_text(json.dumps(state), encoding="utf-8")  # a failure later keeps what's done
    return conversations, added, skipped, updated
