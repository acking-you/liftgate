"""Unit tests for Liftgate Desktop release manifest and SHA256SUMS generation."""

import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from ui_release_manifest import CHECKSUMS, EXPECTED, build, read_workspace_version, sign, verify


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


if __name__ == "__main__":
    unittest.main()
