#!/usr/bin/env python3
"""Claude Code / Codex hook that records every prompt and answer in prompt-analyzer.

Usage (set up by install.py):  python3 hook.py claude|codex [API_URL]   < hook JSON on stdin

It never blocks the agent: prints nothing, always exits 0, and logs failures to
~/.prompt-analyzer/hook.log.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

API = (sys.argv[2] if len(sys.argv) > 2 else os.environ.get("PROMPT_ANALYZER_URL", "http://localhost:8080")).rstrip("/")
LOG = Path.home() / ".prompt-analyzer" / "hook.log"


def post(path, body):
    req = urllib.request.Request(API + path, json.dumps(body).encode(), {"content-type": "application/json"})
    urllib.request.urlopen(req, timeout=3).read()


def local_path(path):
    """Windows apps run this hook through wsl.exe and pass C:\\... paths; read them via /mnt/c."""
    if isinstance(path, str):
        path = path.removeprefix("\\\\?\\")
        if len(path) > 2 and path[1] == ":" and path[2] in "\\/":
            return f"/mnt/{path[0].lower()}{path[2:]}".replace("\\", "/")
    return path


def read_jsonl(path):
    path = local_path(path)
    entries = []
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                try:
                    entries.append(json.loads(line))
                except ValueError:
                    pass
    except (OSError, TypeError):
        pass
    return entries


def is_human_prompt(e):
    if e.get("type") != "user" or e.get("isSidechain") or e.get("isMeta"):
        return False
    origin = e.get("origin")
    if origin is not None:
        return origin.get("kind") == "human"
    content = (e.get("message") or {}).get("content")
    return isinstance(content, str) or not any(b.get("type") == "tool_result" for b in content or [])


def is_final_answer(e):
    m = e.get("message") or {}
    return e.get("type") == "assistant" and not e.get("isSidechain") and m.get("stop_reason") not in (None, "tool_use")


def claude_turn(transcript):
    """Usage, final text and model of the last Claude Code turn, plus whether its final message
    (stop_reason other than tool_use) has been written yet. The turn starts after the previous
    finished answer, so an interrupted attempt before the last prompt is counted too."""
    entries = read_jsonl(transcript)
    last_prompt = max((i for i, e in enumerate(entries) if is_human_prompt(e)), default=-1)
    start = max((i for i, e in enumerate(entries[:max(last_prompt, 0)]) if is_final_answer(e)), default=-1)
    usage, texts, model, done = {}, {}, None, False
    for e in entries[start + 1:]:
        if e.get("type") != "assistant" or e.get("isSidechain"):
            continue
        m = e.get("message") or {}
        # One API message is streamed as several lines sharing an id; count its usage once.
        usage[m.get("id")] = m.get("usage") or {}
        model = m.get("model") or model
        done = done or m.get("stop_reason") not in (None, "tool_use")
        text = "".join(b.get("text", "") for b in m.get("content") or [] if b.get("type") == "text")
        if text:
            texts[m.get("id")] = texts.get(m.get("id"), "") + text
    u = usage.values()
    cached = sum(x.get("cache_read_input_tokens", 0) for x in u)
    inp = sum(x.get("input_tokens", 0) + x.get("cache_creation_input_tokens", 0) for x in u) + cached
    out = sum(x.get("output_tokens", 0) for x in u)
    return inp, out, cached, (list(texts.values()) or [""])[-1], model, done


def codex_turn(transcript, turn_id):
    """Token usage of one Codex turn: the growth of total_token_usage across it."""
    before, after, inside = {}, None, False
    for e in read_jsonl(transcript):
        p = e.get("payload") or {}
        kind = p.get("type")
        if kind == "task_started" and p.get("turn_id") == turn_id:
            inside = True
        elif kind == "token_count" and (p.get("info") or {}).get("total_token_usage"):
            if inside:
                after = p["info"]["total_token_usage"]
            else:
                before = p["info"]["total_token_usage"]
        elif kind in ("task_complete", "turn_aborted") and inside and p.get("turn_id") == turn_id:
            break
    if after is None:
        return None
    grew = lambda k: max(0, after.get(k, 0) - before.get(k, 0))
    return grew("input_tokens"), grew("output_tokens"), grew("cached_input_tokens")


# Prompts the Codex app sends on its own (not typed by the user).
CODEX_INTERNAL_PROMPTS = ("# Overview\n\nGenerate 0 to 3 hyperpersonalized suggestions",)


def is_internal(source, event):
    """Codex app background threads (e.g. suggestions) keep no transcript; skip them and their known prompts."""
    if source != "codex":
        return False
    prompt = (event.get("prompt") or "").lstrip()
    return not event.get("transcript_path") or prompt.startswith(CODEX_INTERNAL_PROMPTS)


def main():
    source = sys.argv[1] if len(sys.argv) > 1 else "claude"
    event = json.load(sys.stdin)
    session_id = event.get("session_id")
    if not session_id or is_internal(source, event):
        return
    name = event.get("hook_event_name")
    if name == "UserPromptSubmit":
        post("/v1/track/prompt", {"source": source, "external_id": session_id, "prompt": event.get("prompt", ""), "model": event.get("model")})
    elif name == "Stop":
        transcript = event.get("transcript_path")
        if source == "codex":
            usage = codex_turn(transcript, event.get("turn_id"))
            if usage is None:  # the last token count may land a moment after Stop
                time.sleep(1)
                usage = codex_turn(transcript, event.get("turn_id")) or (0, 0, 0)
            (inp, out, cached), text, model = usage, event.get("last_assistant_message") or "", event.get("model")
        else:
            for _ in range(6):  # the final message can reach the transcript a moment after Stop
                inp, out, cached, text, model, done = claude_turn(transcript)
                if done:
                    break
                time.sleep(0.5)
        try:
            post("/v1/track/complete", {
                "source": source, "external_id": session_id, "response": text,
                "input_tokens": inp, "output_tokens": out, "cached_tokens": cached, "model": model,
            })
        except urllib.error.HTTPError as e:
            if e.code != 404:  # 404: the turn was only a rating and got folded into the previous answer
                raise


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # never break the agent
        try:
            LOG.parent.mkdir(parents=True, exist_ok=True)
            if LOG.exists() and LOG.stat().st_size > 1_000_000:
                LOG.write_text("")
            with LOG.open("a", encoding="utf-8") as f:
                f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')} {sys.argv[1:]} {type(e).__name__}: {e}\n")
        except OSError:
            pass
    sys.exit(0)
