# AGENTS.md

## GUI interaction testing

Meeting Master is a GPUI desktop app. To exercise the real UI, use the Codex Computer Use
bridge (works from any agent with a shell); details in `docs/agent-gui-testing.md`.

```bash
scripts/dev-app.sh launch                                  # build, bundle, (re)start the app
scripts/gui_agent.py run "<task with observable expectations>"   # JSON report on stdout
```

Exit codes: 0 pass, 1 fail, 2 blocked, 3 harness error. On `blocked` with
"Computer Use was not approved", stop and ask the human to do the one-time approval in the
doc; do not try to bypass it. After UI changes, relaunch before testing.

Harness unit tests: `python3 -m unittest discover -s scripts/tests`.
