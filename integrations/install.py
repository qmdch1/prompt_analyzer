#!/usr/bin/env python3
"""Connect Claude Code and Codex to prompt-analyzer: hooks record every turn, MCP adds rating/stats tools.

    python3 integrations/install.py              # install (safe to re-run)
    python3 integrations/install.py --check      # is everything connected and recording?
    python3 integrations/install.py --uninstall  # remove everything again

Set PROMPT_ANALYZER_URL if the API is not at http://localhost:8080.
"""
import json
import os
import shutil
import subprocess
import sys
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

HOME = Path.home()
API = os.environ.get("PROMPT_ANALYZER_URL", "http://localhost:8080").rstrip("/")
HOOK = HOME / ".prompt-analyzer" / "hook.py"
EVENTS = ("UserPromptSubmit", "Stop")
AGENTS = {
    "claude": {"hooks": HOME / ".claude" / "settings.json",
               "mcp_add": ["claude", "mcp", "add", "--scope", "user", "--transport", "http", "prompt-analyzer", f"{API}/mcp/claude"],
               "mcp_remove": ["claude", "mcp", "remove", "--scope", "user", "prompt-analyzer"]},
    "codex": {"hooks": HOME / ".codex" / "hooks.json",
              "mcp_add": ["codex", "mcp", "add", "prompt-analyzer", "--url", f"{API}/mcp/codex"],
              "mcp_remove": ["codex", "mcp", "remove", "prompt-analyzer"]},
}


def edit_hooks(path, source, install):
    """Adds or removes our hook entries, leaving every other hook untouched."""
    data = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    hooks = data.setdefault("hooks", {})
    for event in EVENTS:
        groups = [g for g in hooks.get(event, []) if not any(str(HOOK) in h.get("command", "") for h in g.get("hooks", []))]
        if install:
            groups.append({"hooks": [{"type": "command", "command": f'python3 "{HOOK}" {source} {API}', "timeout": 10}]})
        if groups:
            hooks[event] = groups
        else:
            hooks.pop(event, None)
    if not hooks:
        data.pop("hooks")
    backup = path.with_name(path.name + ".prompt-analyzer.bak")
    if path.exists() and not backup.exists():  # keep the pre-install original
        shutil.copy(path, backup)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def run(cmd):
    return subprocess.run(cmd, capture_output=True, text=True).returncode == 0


def our_groups(path):
    """{event: index of our hook group} as registered in an agent's hook config."""
    try:
        hooks = json.loads(path.read_text(encoding="utf-8")).get("hooks", {})
    except (OSError, ValueError):
        return {}
    return {event: i for event in EVENTS for i, g in enumerate(hooks.get(event, []))
            if any(str(HOOK) in h.get("command", "") for h in g.get("hooks", []))}


def check(found):
    def line(good, text, fix=None):
        print(f"  {'✓' if good else '✗'} {text}" + (f"\n      → {fix}" if fix and not good else ""))
        return good
    def get(path):
        with urllib.request.urlopen(API + path, timeout=3) as r:
            return json.loads(r.read())
    print("[공통]")
    try:
        server = get("/health").get("status") == "ok"
    except Exception:
        server = False
    line(server, f"서버 켜짐 ({API})", "저장소 폴더에서: docker compose up -d")
    if line(HOOK.exists(), f"훅 스크립트 설치됨 ({HOOK})", "python3 integrations/install.py"):
        line(HOOK.read_bytes() == Path(__file__).with_name("hook.py").read_bytes(), "훅 스크립트가 최신", "python3 integrations/install.py 다시 실행")
    for name, agent in found.items():
        print(f"\n[{name}]")
        groups = our_groups(agent["hooks"])
        line(len(groups) == len(EVENTS), f"훅 등록 ({', '.join(EVENTS)})", "python3 integrations/install.py")
        if name == "codex" and groups:
            # Codex runs a new hook only after the user trusts it once; trust is keyed by file:event:group:handler.
            config = (HOME / ".codex" / "config.toml").read_text(encoding="utf-8") if (HOME / ".codex" / "config.toml").exists() else ""
            snake = {"UserPromptSubmit": "user_prompt_submit", "Stop": "stop"}
            trusted = all(f'"{agent["hooks"]}:{snake[e]}:{i}:0"' in config for e, i in groups.items())
            line(trusted, "Codex에서 훅 승인됨", "codex 를 한 번 켜서 새 훅을 신뢰(승인)하세요")
        if shutil.which(name):
            out = subprocess.run([name, "mcp", "get", "prompt-analyzer"], capture_output=True, text=True).stdout
            good = ("Connected" in out) if name == "claude" else ("enabled: true" in out)
            line(good, "MCP 연결 (rate_last_answer, prompt_stats)", "python3 integrations/install.py")
        if server:
            items = get(f"/v1/prompts?source={name}&limit=1").get("items") or []
            if items:
                ago = datetime.now(timezone.utc) - datetime.fromisoformat(items[0]["created_at"])
                mins = int(ago.total_seconds() // 60)
                print(f"  · 마지막 기록: {'방금' if mins < 1 else f'{mins}분 전' if mins < 60 else f'{mins // 60}시간 전' if mins < 1440 else f'{mins // 1440}일 전'} — {items[0]['prompt'][:40]!r}")
            else:
                print(f"  · 아직 기록 없음 — {name}를 새로 시작해서 프롬프트를 하나 보내 보세요")
    log = HOOK.parent / "hook.log"
    if log.exists() and log.stat().st_size:
        print("\n[최근 훅 오류] " + str(log))
        print("".join(log.read_text(encoding="utf-8").splitlines(keepends=True)[-3:]), end="")


def main():
    install = "--uninstall" not in sys.argv
    found = {name: a for name, a in AGENTS.items() if shutil.which(name) or a["hooks"].parent.exists()}
    if not found:
        sys.exit("Claude Code도 Codex도 찾지 못했습니다.")
    if "--check" in sys.argv:
        return check(found)
    if install:
        HOOK.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(Path(__file__).with_name("hook.py"), HOOK)
    for name, agent in found.items():
        edit_hooks(agent["hooks"], name, install)
        mcp = "CLI 없음"
        if shutil.which(name):
            run(agent["mcp_remove"])
            mcp = ("연결" if run(agent["mcp_add"]) else "실패") if install else "제거"
        print(f"{name:<7} 훅 {'설치' if install else '제거'} ({agent['hooks']}), MCP {mcp}")
    if not install:
        shutil.rmtree(HOOK.parent, ignore_errors=True)
        print("\n연결을 모두 해제했습니다.")
        return
    print(f"\n완료. 에이전트를 새로 시작하면 모든 프롬프트가 {API} 에 자동 기록됩니다.")
    if "codex" in found:
        print("Codex는 처음 시작할 때 새 훅을 신뢰할지 묻습니다. 한 번 승인해 주세요.")


if __name__ == "__main__":
    main()
