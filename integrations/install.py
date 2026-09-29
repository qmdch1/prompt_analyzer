#!/usr/bin/env python3
"""Connect Claude Code and Codex to prompt-analyzer: hooks record every turn, MCP adds rating/stats tools.

    python3 integrations/install.py              # install (safe to re-run)
    python3 integrations/install.py --check      # is everything connected and recording?
    python3 integrations/install.py --import     # import past turns again (install already does it once)
    python3 integrations/install.py --uninstall  # remove everything again

Run it where the hook should live (Linux, macOS, or WSL). From WSL it also connects the Windows
Claude Code / Codex apps, which then run the same hook through wsl.exe.
Set PROMPT_ANALYZER_URL if the API is not at http://localhost:8080.
"""
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import history  # noqa: E402  (lives next to this file, in the repo and in ~/.prompt-analyzer)

HOME = Path.home()
API = os.environ.get("PROMPT_ANALYZER_URL", "http://localhost:8080").rstrip("/")
HOOK = HOME / ".prompt-analyzer" / "hook.py"
EVENTS = ("UserPromptSubmit", "Stop")


def windows_home():
    """C:\\Users\\<me> as a WSL path, or None when not running inside WSL."""
    if not os.environ.get("WSL_DISTRO_NAME") or not shutil.which("cmd.exe"):
        return None
    try:
        win = subprocess.run(["cmd.exe", "/c", "echo %USERPROFILE%"], capture_output=True, text=True, cwd="/mnt/c", timeout=15).stdout.strip()
        path = subprocess.run(["wslpath", "-u", win], capture_output=True, text=True, timeout=15).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return None
    return Path(path) if path and Path(path).is_dir() else None


def agents():
    """Every agent install this machine has: (label, source, hook config file, hook command, CLI or None)."""
    found = []
    for source, config in (("claude", HOME / ".claude" / "settings.json"), ("codex", HOME / ".codex" / "hooks.json")):
        if shutil.which(source) or config.parent.exists():
            # `|| true`: a hook that can't run must never block the prompt (exit code 2 means "block").
            found.append((source, source, config, f'python3 "{HOOK}" {source} {API} || true', shutil.which(source)))
    win = windows_home()
    if win:
        # Windows apps hand the hook JSON to this distro. The path stays inside quotes as ~/...: Git Bash
        # (Claude Code on Windows) rewrites bare /home/... arguments into C:/Program Files/Git/home/...
        via_wsl = f'wsl.exe -d {os.environ["WSL_DISTRO_NAME"]} -e sh -c "python3 ~/{HOOK.relative_to(HOME)}'
        codex_exes = sorted(win.glob("AppData/Local/OpenAI/Codex/bin/*/codex.exe"), key=lambda p: p.stat().st_mtime)
        for source, config, cli in (("claude", win / ".claude" / "settings.json", win / ".local" / "bin" / "claude.exe"),
                                    ("codex", win / ".codex" / "hooks.json", codex_exes[-1] if codex_exes else None)):
            if config.parent.exists():
                found.append((f"{source} (Windows)", source, config, f'{via_wsl} {source} {API} || true"', str(cli) if cli and Path(cli).exists() else None))
    return found


def ours(hook):
    return f"{HOOK.parent.name}/{HOOK.name}" in hook.get("command", "")


def edit_hooks(path, command, install):
    """Adds or removes our hook entries, leaving every other hook untouched."""
    data = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    hooks = data.setdefault("hooks", {})
    for event in EVENTS:
        groups = [g for g in hooks.get(event, []) if not any(ours(h) for h in g.get("hooks", []))]
        if install:
            groups.append({"hooks": [{"type": "command", "command": command, "timeout": 10}]})
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


def cli(exe, *args):
    # Windows executables get a Windows working directory; a \\wsl$ path confuses some of them.
    return subprocess.run([exe, *args], capture_output=True, text=True, cwd="/mnt/c" if exe.endswith(".exe") else None)


def mcp(exe, source, install):
    scope = ["--scope", "user"] if source == "claude" else []
    cli(exe, "mcp", "remove", *scope, "prompt-analyzer")
    if not install:
        return "제거"
    add = ["--transport", "http", "prompt-analyzer", f"{API}/mcp/{source}"] if source == "claude" else ["prompt-analyzer", "--url", f"{API}/mcp/{source}"]
    return "연결" if cli(exe, "mcp", "add", *scope, *add).returncode == 0 else "실패"


def our_groups(path):
    """{event: index of our hook group} as registered in an agent's hook config."""
    try:
        hooks = json.loads(path.read_text(encoding="utf-8")).get("hooks", {})
    except (OSError, ValueError):
        return {}
    return {event: i for event in EVENTS for i, g in enumerate(hooks.get(event, [])) if any(ours(h) for h in g.get("hooks", []))}


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
    for label, source, config, _, exe in found:
        print(f"\n[{label}]  {config}")
        groups = our_groups(config)
        line(len(groups) == len(EVENTS), f"훅 등록 ({', '.join(EVENTS)})", "python3 integrations/install.py")
        if source == "codex" and groups:
            # Codex runs a new hook only after the user trusts it once; trust is keyed by file:event:group:handler.
            toml = config.with_name("config.toml")
            text = toml.read_text(encoding="utf-8") if toml.exists() else ""
            snake = {"UserPromptSubmit": "user_prompt_submit", "Stop": "stop"}
            trusted = all(re.search(rf'hooks\.json:{snake[e]}:{i}:0["\']\]\s*trusted_hash', text) for e, i in groups.items())
            # Only presence is checkable here; a changed command needs approval again, so "마지막 기록" below is the real proof.
            line(trusted, "Codex 훅 승인 기록 있음", "Codex를 새로 켜서 새 훅을 신뢰(승인)하세요 (앱은 설정의 훅 화면)")
        if exe:
            out = cli(exe, "mcp", "get", "prompt-analyzer").stdout
            line(("Connected" in out) if source == "claude" else ("enabled: true" in out), "MCP 연결 (rate_last_answer, check_connection, prompt_stats)", "python3 integrations/install.py")
    if server:
        print()
        for source in sorted({f[1] for f in found}):
            items = get(f"/v1/prompts?source={source}&limit=1").get("items") or []
            if items:
                mins = int((datetime.now(timezone.utc) - datetime.fromisoformat(items[0]["created_at"])).total_seconds() // 60)
                when = "방금" if mins < 1 else f"{mins}분 전" if mins < 60 else f"{mins // 60}시간 전" if mins < 1440 else f"{mins // 1440}일 전"
                print(f"  · {source} 마지막 기록: {when} — {items[0]['prompt'][:40]!r}")
            else:
                print(f"  · {source} 아직 기록 없음 — 새로 시작해서 프롬프트를 하나 보내 보세요")
    imported = sorted(HOOK.parent.glob("imported-*.json"), key=lambda p: p.stat().st_mtime)
    if imported:
        mins = int((datetime.now().timestamp() - imported[-1].stat().st_mtime) // 60)
        print(f"  · 과거 기록 가져오기: 마지막 실행 {'방금' if mins < 1 else f'{mins}분 전' if mins < 60 else f'{mins // 60}시간 전'} (훅이 1시간마다 빠진 기록을 채움)")
    log = HOOK.parent / "hook.log"
    if log.exists() and log.stat().st_size:
        print("\n[최근 훅 오류] " + str(log))
        print("".join(log.read_text(encoding="utf-8").splitlines(keepends=True)[-3:]), end="")


def import_history():
    """Past turns from every transcript folder this machine has (see history.py)."""
    win = windows_home()
    result = history.run(API, [HOME] + ([win] if win else []))
    if "--quiet" in sys.argv:
        return
    if result is None:
        print("과거 기록: 다른 가져오기가 이미 실행 중입니다.")
    else:
        conversations, added, skipped, updated = result
        fixed = f" (그중 {updated}개는 대화 기록 기준으로 토큰을 채우거나 바로잡음)" if updated else ""
        print(f"과거 기록: 대화 {conversations}개에서 새로 {added}개를 가져왔고, 이미 있던 {skipped}개는 건너뛰었습니다{fixed}.")


def main():
    if "--import" in sys.argv:
        return import_history()
    install = "--uninstall" not in sys.argv
    found = agents()
    if not found:
        sys.exit("Claude Code도 Codex도 찾지 못했습니다.")
    if "--check" in sys.argv:
        return check(found)
    if install:
        HOOK.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(Path(__file__).with_name("hook.py"), HOOK)
        shutil.copy(Path(__file__).with_name("history.py"), HOOK.parent / "history.py")
        if Path(__file__).resolve() != (HOOK.parent / "install.py").resolve():  # so --check works from anywhere
            shutil.copy(__file__, HOOK.parent / "install.py")
    for label, source, config, command, exe in found:
        edit_hooks(config, command, install)
        print(f"{label:<17} 훅 {'설치' if install else '제거'} ({config}), MCP {mcp(exe, source, install) if exe else 'CLI 없음'}")
    if not install:
        shutil.rmtree(HOOK.parent, ignore_errors=True)
        print("\n연결을 모두 해제했습니다.")
        return
    print()
    try:
        import_history()
    except OSError as e:  # server down: the hooks will fill the history in later
        print(f"과거 기록: 서버에 연결하지 못해 건너뛰었습니다 ({e}). 서버를 켜 두면 훅이 1시간 안에 가져옵니다.")
    print(f"\n완료. 에이전트를 새로 시작하면 모든 프롬프트가 {API} 에 자동 기록됩니다.")
    if any(source == "codex" for _, source, *_ in found):
        print("Codex는 처음 시작할 때 새 훅을 신뢰할지 묻습니다. 한 번 승인해 주세요.")


if __name__ == "__main__":
    main()
