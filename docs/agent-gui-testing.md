# Agent-driven GUI testing (Codex Computer Use)

Meeting Master is a GPUI desktop app, so unit tests cannot cover real interaction.
This harness lets **any** coding agent (Claude Code, Codex, OpenCode, CI scripts…) drive the
running app through OpenAI's Codex Computer Use and get a structured pass/fail report.

## How it works

```
your agent ──shell──▶ scripts/gui_agent.py run "<task>"
                          │  codex exec (headless, read-only, JSON-schema output)
                          ▼
                     Codex ──cua_repl──▶ Codex Computer Use service ──AX/screenshots──▶ Meeting Master Dev.app
```

- The Computer Use service (`~/.codex/computer-use/Codex Computer Use.app`) only accepts
  authenticated callers. Its MCP client (`SkyComputerUseClient mcp`) launched from a non-Codex
  process fails with `Sender process is not authenticated`. So Codex is the bridge: we
  delegate to `codex exec` instead of wiring the MCP server into other agents.
- Computer Use addresses apps by name, bundle id or `.app` path and rejects bare cargo
  binaries (`Invalid app`). `scripts/dev-app.sh` wraps the debug build into
  `target/debug/MeetingMasterDev.app` with the stable bundle id `dev.meetingmaster.app.dev`,
  so the one-time approval keeps working across rebuilds.

## One-time setup (human)

1. Codex CLI logged in, with the Computer Use plugin installed (Codex desktop app → Plugins).
2. Launch the dev app: `scripts/dev-app.sh launch`.
3. Approve Computer Use for the app. Headless `codex exec` auto-declines the approval prompt,
   so do it once interactively:

   ```bash
   codex -C . "Use Computer Use to get the app state of 'Meeting Master Dev'"
   ```

   and choose **Always allow** when asked about *Meeting Master Dev*. The bridge then reuses
   that stored approval by running `codex exec -c approval_policy="on-request"`: exec's default
   policy (`never`) makes Computer Use reject even approved apps.
4. Verify: `scripts/gui_agent.py check`, then a smoke test (below) must return `pass`.

## Usage (agents)

```bash
scripts/dev-app.sh launch                       # rebuild + (re)start the app
scripts/gui_agent.py run "Verify the main window shows the text 'Meeting Master'."
scripts/gui_agent.py run --launch "<task>"      # rebuild + relaunch, then test
echo "<long multi-line task>" | scripts/gui_agent.py run -
scripts/dev-app.sh stop
```

stdout is a JSON report:

```json
{"status": "pass|fail|blocked", "summary": "…", "steps": [{"action": "…", "observation": "…"}],
 "issues": ["…"], "final_state": "…"}
```

Exit codes: `0` pass · `1` fail (app misbehaved) · `2` blocked (could not test, e.g. not
approved / app not running) · `3` harness error (codex missing, timeout, malformed output).

### Writing good tasks

- State concrete, observable expectations ("the list shows 3 rows", "a dialog titled X
  appears"), not vague goals. The executor decides pass/fail from them.
- One scenario per run; chain runs for longer flows (each run is a fresh Codex session).
- Mention the UI by its visible text. GPUI currently exposes **no** app content to the macOS
  accessibility tree (only window chrome and the menu bar), so the executor works purely from
  screenshots and pixel coordinates; visible labels make that reliable.

## Limits

- macOS only; the app must be on screen (Computer Use operates the real window).
- Each run costs a Codex model session (~30k tokens for a trivial check, ~1 min).
- Codex runs `-s read-only` and is told to touch only the dev app; it cannot edit the repo.

## 隔离数据与系统权限（2026-11-05 实测）

- Codex Computer Use 有时会结束已运行的实例并自己重新启动 App；它启动的实例不会继承 `open --env` 设置的环境变量，于是会用真实的数据目录并自动发现你的 Obsidian 库。测试时请用会话级环境变量，测完立即撤销：

  ```bash
  launchctl setenv MM_DATA_DIR /path/to/scratch/data
  ```

  ```bash
  launchctl unsetenv MM_DATA_DIR
  ```

- `Computer Use server error -10005: noWindowsAvailable` 是间歇性的：先 `osascript -e 'tell application id "dev.meetingmaster.app.dev" to activate'`，确认只有一个实例在运行，再重试。
- TCC 按签名的"指定要求"识别 App。`scripts/dev-app.sh` 默认用钥匙串里第一个 `Apple Development` 身份签名，指定要求只包含 bundle id 和证书，重建后不变，所以麦克风、屏幕与系统录音只需授权一次。第一次签名时钥匙串会询问 codesign 能否使用私钥，选"始终允许"。
- 没有 `Apple Development` 身份时，脚本会提示并改用 ad-hoc 签名。这时指定要求是 cdhash，每次重建都会被当成新 App，需要重新授权。没有开发者账号的 Mac 可以在"钥匙串访问"里自建一张代码签名证书，再用 `MM_SIGN_IDENTITY` 指定它。用 `scripts/dev-app.sh sign-identity` 查看当前使用的身份，用 `codesign -d -r- "$(scripts/dev-app.sh path)"` 确认指定要求里没有 `cdhash`。
- 换了签名身份（比如从 ad-hoc 换成证书）之后，需要再授权一次。要从头测授权流程，只重置开发包，例如 `tccutil reset Microphone dev.meetingmaster.app.dev`。
- 从终端或 Claude 启动的进程，权限算在启动它的 App（终端、Claude）头上，而不是 Meeting Master Dev。
