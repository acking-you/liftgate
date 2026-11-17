"""Unit tests for Liftgate Desktop release manifest and SHA256SUMS generation."""

import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

from ui_release_manifest import CHECKSUMS, EXPECTED, build, read_workspace_version, sign, verify

REPO_ROOT = Path(__file__).resolve().parents[2]


class DesktopReleaseManifestTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def test_desktop_release_manifest_verifies_complete_asset_set_and_cargo_version(self):
        for name in EXPECTED:
            (self.directory / name).write_bytes(f"payload-{name}".encode("utf-8"))

        version = read_workspace_version(
            Path(__file__).resolve().parents[2] / "Cargo.toml"
        )
        manifest_path = build(self.directory, f"v{version}", "deadbeef")
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

        self.assertEqual(manifest["version"], version)
        self.assertEqual(manifest["tag"], f"v{version}-ui")
        self.assertEqual(manifest["commit"], "deadbeef")
        self.assertEqual({item["name"] for item in manifest["assets"]}, EXPECTED)

        checksums = (self.directory / CHECKSUMS).read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(checksums), len(EXPECTED))

        # Missing asset must fail closed.
        (self.directory / next(iter(EXPECTED))).unlink()
        with self.assertRaises(SystemExit):
            build(self.directory, f"v{version}", "deadbeef")

    def test_mismatched_tag_or_empty_file_fails_closed(self):
        for name in EXPECTED:
            (self.directory / name).write_bytes(b"nonempty")

        with self.assertRaises(SystemExit):
            build(self.directory, "v999.999.999", "deadbeef")

        version = read_workspace_version(
            Path(__file__).resolve().parents[2] / "Cargo.toml"
        )
        sample = self.directory / next(iter(EXPECTED))
        sample.write_bytes(b"")
        with self.assertRaises(SystemExit):
            build(self.directory, f"v{version}", "deadbeef")

    @unittest.skipUnless(shutil.which("openssl"), "openssl CLI not on PATH")
    def test_ed25519_sign_and_verify_roundtrip(self):
        for name in EXPECTED:
            (self.directory / name).write_bytes(b"binary")
        version = read_workspace_version(
            Path(__file__).resolve().parents[2] / "Cargo.toml"
        )
        manifest_path = build(self.directory, f"v{version}", "deadbeef")

        priv = self.directory / "ed25519.pem"
        pub = self.directory / "ed25519.pub.pem"
        subprocess.run(
            ["openssl", "genpkey", "-algorithm", "ED25519", "-out", str(priv)],
            check=True,
        )
        subprocess.run(
            ["openssl", "pkey", "-in", str(priv), "-pubout", "-out", str(pub)],
            check=True,
        )
        sig_path = sign(manifest_path, priv)
        self.assertTrue(sig_path.is_file())
        verify(manifest_path, pub)

        # Tampering with manifest must invalidate signature.
        manifest_path.write_text(manifest_path.read_text(encoding="utf-8") + "\n", encoding="utf-8")
        with self.assertRaises(subprocess.CalledProcessError):
            verify(manifest_path, pub)

    def test_readmes_link_current_desktop_release_and_cli_installers(self):
        version = read_workspace_version(REPO_ROOT / "Cargo.toml")
        ui_tag = f"v{version}-ui"
        required_snippets = (
            "curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh",
            "irm https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.ps1 | iex",
            f"https://github.com/acking-you/liftgate/releases/tag/{ui_tag}",
            f"https://github.com/acking-you/liftgate/releases/download/{ui_tag}/liftgate-desktop-macos-arm64.dmg",
            f"https://github.com/acking-you/liftgate/releases/download/{ui_tag}/liftgate-desktop-macos-x86_64.dmg",
            f"https://github.com/acking-you/liftgate/releases/download/{ui_tag}/liftgate-desktop-windows-x86_64-setup.exe",
            f"https://github.com/acking-you/liftgate/releases/download/{ui_tag}/liftgate-desktop-windows-arm64-setup.exe",
            f"https://github.com/acking-you/liftgate/releases/download/{ui_tag}/liftgate-desktop-linux-x86_64.AppImage",
            f"https://github.com/acking-you/liftgate/releases/download/{ui_tag}/liftgate-desktop-linux-amd64.deb",
        )
        for doc_name in ("README.md", "README.zh-CN.md"):
            content = (REPO_ROOT / doc_name).read_text(encoding="utf-8")
            for snippet in required_snippets:
                self.assertIn(snippet, content, f"{doc_name} missing {snippet}")
            found_ui_tags = set(re.findall(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.]+)?-ui", content))
            self.assertEqual(
                found_ui_tags,
                {ui_tag},
                f"{doc_name} references stale desktop tag(s): {found_ui_tags} (expected only {ui_tag})",
            )


if __name__ == "__main__":
    unittest.main()
