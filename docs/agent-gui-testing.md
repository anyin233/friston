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
