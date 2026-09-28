#!/usr/bin/env python3
"""Connect Claude Code and Codex to prompt-analyzer: hooks record every turn, MCP adds rating/stats tools.

    python3 integrations/install.py              # install (safe to re-run)
    python3 integrations/install.py --uninstall  # remove everything again

Set PROMPT_ANALYZER_URL if the API is not at http://localhost:8080.
"""
import json
import os
import shutil
import subprocess
import sys
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


def main():
    install = "--uninstall" not in sys.argv
    found = {name: a for name, a in AGENTS.items() if shutil.which(name) or a["hooks"].parent.exists()}
    if not found:
        sys.exit("Claude Code도 Codex도 찾지 못했습니다.")
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
