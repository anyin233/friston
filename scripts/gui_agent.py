#!/usr/bin/env python3
"""Agent-agnostic GUI test bridge: drive Friston through Codex Computer Use.

Any agent (Claude Code, Codex, OpenCode, a CI job…) that can run a shell command can
use this to interact with the running GUI. It hands a natural-language task to a
headless ``codex exec`` session, which is the only process allowed to talk to the
Codex Computer Use service (that service authenticates its caller, so a generic MCP
client cannot use it directly). Codex returns a schema-constrained JSON report.

Stdlib only, so it runs with any python3 without a virtualenv.

Usage:
    scripts/gui_agent.py check
    scripts/gui_agent.py run "Click 'New meeting' and verify the editor opens" [--launch]

Exit codes: 0 pass, 1 fail, 2 blocked (could not perform the test), 3 harness error.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DEV_APP_SCRIPT = REPO_ROOT / "scripts" / "dev-app.sh"
DEFAULT_BUNDLE_ID = os.environ.get("MM_DEV_BUNDLE_ID", "dev.meetingmaster.app.dev")
CUA_SERVICE_APP = Path.home() / ".codex" / "computer-use" / "Codex Computer Use.app"
STATUSES = ("pass", "fail", "blocked")
HARNESS_ERROR = 3

REPORT_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["status", "summary", "steps", "issues", "final_state"],
    "properties": {
        "status": {"type": "string", "enum": list(STATUSES)},
        "summary": {"type": "string"},
        "steps": {
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": False,
                "required": ["action", "observation"],
                "properties": {
                    "action": {"type": "string"},
                    "observation": {"type": "string"},
                },
            },
        },
        "issues": {"type": "array", "items": {"type": "string"}},
        "final_state": {"type": "string"},
    },
}


class ReportError(ValueError):
    """Raised when Codex's final message is not a valid test report."""


def build_prompt(task: str, bundle_id: str) -> str:
    """Wrap a caller's task in the fixed instructions that scope Codex to the dev app.

    Args:
        task: Natural-language test task from the calling agent, passed verbatim.
        bundle_id: Bundle identifier of the dev app bundle Codex must operate on.

    Returns:
        The full prompt for ``codex exec``.
    """
    return f"""You are a GUI test executor for the Friston desktop app under development.

Target app: bundle id `{bundle_id}` (name "Friston Dev"). It is already running.
Use Computer Use to operate it. Do not interact with any other app, do not edit files,
and do not run shell commands that change state.

The app is built with GPUI, whose accessibility tree may be sparse: if elements are
missing from the tree, act on screenshot pixel coordinates instead.

Task:
<task>
{task}
</task>

Perform the task, observing the UI after every action. Then answer ONLY with JSON
matching the provided schema:
- status: "pass" if every expectation in the task held, "fail" if the app behaved
  incorrectly, "blocked" if you could not carry out the test (app missing, Computer Use
  not approved or unavailable, ambiguous task). Include any error text verbatim.
- steps: each action you took and what you observed right after it.
- issues: concrete defects or blockers, each self-contained.
- final_state: a precise description of the visible UI at the end.
"""


def build_command(
    *, codex: str, repo: Path, schema: Path, out: Path, prompt: str, model: str | None
) -> list[str]:
    """Build the headless ``codex exec`` invocation.

    Args:
        codex: Path or name of the codex CLI.
        repo: Working root for the Codex session.
        schema: JSON schema file constraining the final message.
        out: File Codex writes its final message to.
        prompt: Full prompt from :func:`build_prompt`.
        model: Optional model override; None keeps the user's Codex default.

    Returns:
        argv list for subprocess.
    """
    cmd = [
        codex, "exec",
        "--ephemeral",            # test runs should not pollute the user's session history
        "--skip-git-repo-check",
        "-s", "read-only",        # Codex only needs Computer Use, never to write the repo
        # exec defaults to approval_policy=never, under which Computer Use rejects even
        # persistently approved apps. on-request honours the stored per-app approval; with
        # no TTY, anything that would actually need a prompt is still declined.
        "-c", 'approval_policy="on-request"',
        "-C", str(repo),
        "--output-schema", str(schema),
        "-o", str(out),
    ]
    if model:
        cmd += ["-m", model]
    cmd.append(prompt)
    return cmd


def parse_result(text: str) -> dict:
    """Parse and validate Codex's final message as a test report.

    Args:
        text: Raw final message (may be wrapped in a Markdown code fence).

    Returns:
        The report dict.

    Raises:
        ReportError: If the text is not JSON or lacks a valid status.
    """
    stripped = re.sub(r"^```(?:json)?\s*|\s*```$", "", text.strip())
    try:
        report = json.loads(stripped)
    except json.JSONDecodeError as exc:
        raise ReportError(f"Codex final message is not JSON: {exc}; text={text[:500]!r}") from exc
    if not isinstance(report, dict) or report.get("status") not in STATUSES:
        raise ReportError(f"report has invalid status: {str(report)[:500]}")
    return report


def exit_code_for(status: str) -> int:
    """Map a report status to the process exit code (pass=0, fail=1, blocked=2).

    Args:
        status: One of STATUSES.

    Returns:
        Exit code.
    """
    return STATUSES.index(status)


def check() -> int:
    """Print a preflight checklist for the GUI-testing toolchain.

    Returns:
        0 if every prerequisite is met, otherwise HARNESS_ERROR.
    """
    dev_app = subprocess.run(
        [str(DEV_APP_SCRIPT), "status"], capture_output=True, text=True, check=False
    )
    checks = [
        ("codex CLI on PATH", shutil.which("codex") is not None),
        (f"Codex Computer Use installed ({CUA_SERVICE_APP})", CUA_SERVICE_APP.exists()),
        ("dev app running (scripts/dev-app.sh launch)", dev_app.returncode == 0),
    ]
    for name, ok in checks:
        print(f"[{'ok' if ok else 'MISSING'}] {name}")
    print(
        "note: Computer Use must be approved once for 'Friston Dev' "
        "(see docs/agent-gui-testing.md)."
    )
    return 0 if all(ok for _, ok in checks) else HARNESS_ERROR


def run(task: str, *, launch: bool, model: str | None, timeout: int, bundle_id: str) -> int:
    """Execute one GUI test task through Codex and print the JSON report.

    Args:
        task: Natural-language test task.
        launch: Rebuild and (re)launch the dev app first.
        model: Optional Codex model override.
        timeout: Seconds before the Codex run is aborted.
        bundle_id: Bundle id of the app under test.

    Returns:
        Exit code per :func:`exit_code_for`, or HARNESS_ERROR.
    """
    codex = shutil.which("codex")
    if codex is None:
        print("error: codex CLI not found on PATH", file=sys.stderr)
        return HARNESS_ERROR
    if launch:
        subprocess.run([str(DEV_APP_SCRIPT), "launch"], check=True)

    with tempfile.TemporaryDirectory(prefix="mm-gui-agent-") as tmp:
        schema = Path(tmp) / "schema.json"
        out = Path(tmp) / "last.json"
        schema.write_text(json.dumps(REPORT_SCHEMA))
        cmd = build_command(
            codex=codex, repo=REPO_ROOT, schema=schema, out=out,
            prompt=build_prompt(task, bundle_id), model=model,
        )
        try:
            proc = subprocess.run(
                cmd, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired:
            print(f"error: codex exec timed out after {timeout}s", file=sys.stderr)
            return HARNESS_ERROR
        if proc.returncode != 0 or not out.exists():
            print(
                f"error: codex exec failed (exit {proc.returncode}); stderr tail:\n"
                f"{proc.stderr[-2000:]}",
                file=sys.stderr,
            )
            return HARNESS_ERROR
        try:
            report = parse_result(out.read_text())
        except ReportError as exc:
            print(f"error: {exc}", file=sys.stderr)
            return HARNESS_ERROR

    print(json.dumps(report, ensure_ascii=False, indent=2))
    return exit_code_for(report["status"])


def main(argv: list[str] | None = None) -> int:
    """CLI entry point.

    Args:
        argv: Arguments excluding the program name; defaults to sys.argv[1:].

    Returns:
        Process exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check", help="preflight the toolchain")
    run_p = sub.add_parser("run", help="run one GUI test task")
    run_p.add_argument("task", help="natural-language task, or '-' to read it from stdin")
    run_p.add_argument("--launch", action="store_true", help="rebuild + relaunch the app first")
    run_p.add_argument("--model", help="Codex model override")
    run_p.add_argument("--timeout", type=int, default=600, help="seconds (default 600)")
    run_p.add_argument("--bundle-id", default=DEFAULT_BUNDLE_ID)
    args = parser.parse_args(argv)

    if args.cmd == "check":
        return check()
    task = sys.stdin.read() if args.task == "-" else args.task
    return run(
        task, launch=args.launch, model=args.model, timeout=args.timeout,
        bundle_id=args.bundle_id,
    )


if __name__ == "__main__":
    sys.exit(main())
