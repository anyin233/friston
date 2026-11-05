"""Unit tests for scripts/gui_agent.py (pure logic only; no Codex or GUI required)."""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import gui_agent  # noqa: E402


class BuildPromptTests(unittest.TestCase):
    """The prompt must pin Codex to the dev app and carry the caller's task verbatim."""

    def test_prompt_names_bundle_and_task(self):
        prompt = gui_agent.build_prompt("Click Record and check timer", "dev.x.y")
        self.assertIn("dev.x.y", prompt)
        self.assertIn("Click Record and check timer", prompt)

    def test_prompt_forbids_other_apps(self):
        prompt = gui_agent.build_prompt("task", "dev.x.y")
        self.assertIn("Do not interact with any other app", prompt)


class BuildCommandTests(unittest.TestCase):
    """The codex invocation must be headless, read-only and schema-constrained."""

    def test_command_flags(self):
        cmd = gui_agent.build_command(
            codex="codex", repo=Path("/r"), schema=Path("/s.json"), out=Path("/o.json"),
            prompt="p", model=None,
        )
        self.assertEqual(cmd[:2], ["codex", "exec"])
        for flag in ("--ephemeral", "--skip-git-repo-check", "--output-schema", "-o"):
            self.assertIn(flag, cmd)
        self.assertEqual(cmd[cmd.index("-s") + 1], "read-only")
        # exec defaults to approval_policy=never, under which Computer Use rejects even
        # apps the user has persistently approved.
        self.assertIn('approval_policy="on-request"', cmd)
        self.assertEqual(cmd[-1], "p")
        self.assertNotIn("-m", cmd)

    def test_command_model_override(self):
        cmd = gui_agent.build_command(
            codex="codex", repo=Path("/r"), schema=Path("/s.json"), out=Path("/o.json"),
            prompt="p", model="m1",
        )
        self.assertEqual(cmd[cmd.index("-m") + 1], "m1")


class ParseResultTests(unittest.TestCase):
    """Codex's last message is schema JSON; anything else is reported as an error."""

    def _report(self, status):
        return json.dumps({
            "status": status, "summary": "s", "steps": [], "issues": [], "final_state": "f",
        })

    def test_parses_valid_report(self):
        report = gui_agent.parse_result(self._report("pass"))
        self.assertEqual(report["status"], "pass")

    def test_tolerates_code_fence(self):
        report = gui_agent.parse_result("```json\n" + self._report("fail") + "\n```")
        self.assertEqual(report["status"], "fail")

    def test_rejects_unknown_status(self):
        with self.assertRaises(gui_agent.ReportError):
            gui_agent.parse_result(self._report("maybe"))

    def test_rejects_non_json(self):
        with self.assertRaises(gui_agent.ReportError):
            gui_agent.parse_result("I could not do it")


class ExitCodeTests(unittest.TestCase):
    """Exit codes let any agent branch on the outcome without parsing JSON."""

    def test_mapping(self):
        self.assertEqual(gui_agent.exit_code_for("pass"), 0)
        self.assertEqual(gui_agent.exit_code_for("fail"), 1)
        self.assertEqual(gui_agent.exit_code_for("blocked"), 2)


if __name__ == "__main__":
    unittest.main()
