"""Tests for how scripts/dev-app.sh picks its codesign identity.

A stable identity keeps the TCC designated requirement unchanged across rebuilds; ad-hoc
signing pins it to the cdhash, which forces re-granting microphone/screen access every build.
"""

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "dev-app.sh"

DEV_IDENTITY_OUTPUT = """\
  1) 42926D14DECFD666C1C8FF0E626920554DB50E00 "Apple Development: someone@example.com (5222QY4CWR)"
  2) 1111111111111111111111111111111111111111 "Developer ID Application: Someone (5222QY4CWR)"
     2 valid identities found
"""

NO_DEV_IDENTITY_OUTPUT = """\
  1) 1111111111111111111111111111111111111111 "Developer ID Application: Someone (5222QY4CWR)"
     1 valid identities found
"""


def run_sign_identity(security_output: str, env_overrides: dict) -> str:
    """Run `dev-app.sh sign-identity` with a fake `security` on PATH.

    Args:
        security_output: text the fake `security find-identity` prints.
        env_overrides: extra environment variables (e.g. MM_SIGN_IDENTITY).

    Returns:
        The identity the script printed, stripped.
    """
    with tempfile.TemporaryDirectory() as tmp:
        fake = Path(tmp) / "security"
        listing = Path(tmp) / "listing.txt"
        listing.write_text(security_output)
        fake.write_text(f'#!/bin/sh\ncat "{listing}"\n')
        fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
        env = {k: v for k, v in os.environ.items() if k != "MM_SIGN_IDENTITY"}
        env["PATH"] = f"{tmp}:{env['PATH']}"
        env.update(env_overrides)
        result = subprocess.run(
            [str(SCRIPT), "sign-identity"], env=env, capture_output=True, text=True, check=True
        )
        return result.stdout.strip()


class DevAppSignIdentityTest(unittest.TestCase):
    """Covers the identity fallback order: env override, Apple Development, ad-hoc."""

    def test_prefers_apple_development_identity(self):
        """An Apple Development identity is used by SHA-1 so the requirement stays stable."""
        self.assertEqual(
            run_sign_identity(DEV_IDENTITY_OUTPUT, {}),
            "42926D14DECFD666C1C8FF0E626920554DB50E00",
        )

    def test_env_override_wins(self):
        """MM_SIGN_IDENTITY overrides auto-detection, including "-" to force ad-hoc."""
        self.assertEqual(run_sign_identity(DEV_IDENTITY_OUTPUT, {"MM_SIGN_IDENTITY": "-"}), "-")

    def test_falls_back_to_adhoc(self):
        """Without an Apple Development identity the script still signs, ad-hoc."""
        self.assertEqual(run_sign_identity(NO_DEV_IDENTITY_OUTPUT, {}), "-")


if __name__ == "__main__":
    unittest.main()
