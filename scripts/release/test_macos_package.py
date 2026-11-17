"""Fail-closed macOS packaging checks; no Apple or GitHub credentials required."""

import argparse
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import macos_package


class MacosPackageTests(unittest.TestCase):
    def test_ticket_validation_recovers_from_a_temporary_network_failure(self):
        error = RuntimeError('NSURLErrorDomain Code=-1005 "The network connection was lost."')
        with patch.object(macos_package, "run", side_effect=[error, "validated"]) as run_mock, \
                patch.object(macos_package.time, "sleep") as sleep_mock:
            self.assertEqual(macos_package.stapler("validate", Path("release.dmg")), "validated")
            self.assertEqual(run_mock.call_count, 2)
            sleep_mock.assert_called_once_with(5)

    def test_ticket_retries_are_bounded_and_do_not_retry_invalid_tickets(self):
        for message, attempts in [
            ("NSURLErrorDomain Code=-1005", 4),
            ("Invalid ticket: Error 65", 1),
        ]:
            with self.subTest(message=message):
                error = RuntimeError(message)
                with patch.object(macos_package, "run", side_effect=error) as run_mock, \
                        patch.object(macos_package.time, "sleep") as sleep_mock:
                    with self.assertRaises(RuntimeError) as caught:
                        macos_package.stapler("validate", Path("release.dmg"))
                    self.assertIs(caught.exception, error)
                    self.assertEqual(run_mock.call_count, attempts)
                    self.assertEqual(sleep_mock.call_count, attempts - 1)

    def test_mounted_app_assessment_failure_is_fatal_and_detaches_image(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            info = {
                "CFBundleIdentifier": "top.ackingliu.liftgate.desktop",
                "CFBundleExecutable": "liftgate-desktop",
                "CFBundleShortVersionString": "0.2.0",
                "CFBundleVersion": "0.2.0",
            }
            calls = []

            def fake_run(*args):
                calls.append(args)
                if args[:2] == ("hdiutil", "attach"):
                    contents = work / "mounted/Liftgate.app/Contents"
                    contents.mkdir(parents=True)
                    (contents / "Info.plist").write_bytes(plistlib.dumps(info))
                if args[:2] == ("codesign", "--display"):
                    return "TeamIdentifier=TEAM\nAuthority=Developer ID Application: Test\n"
                if args[0] == "spctl":
                    raise RuntimeError("Gatekeeper rejected mounted application")
                return ""

            with patch.object(macos_package, "run", side_effect=fake_run):
                with self.assertRaisesRegex(RuntimeError, "Gatekeeper rejected"):
                    macos_package.verify_mounted_app(
                        work / "test.dmg", work, info, "TEAM", work, adhoc=False
                    )
            self.assertEqual(calls[-1], ("hdiutil", "detach", work / "mounted"))
            self.assertFalse((work / "mounted-app-verification.txt").exists())

    def test_rejected_notarization_preserves_submission_and_diagnostics(self):
        result = subprocess.CompletedProcess([], 1, '{"id":"rejected-id","status":"Invalid"}', "")
        with tempfile.TemporaryDirectory() as directory:
            logs = Path(directory)
            with patch.object(macos_package.subprocess, "run", return_value=result), \
                    patch.object(macos_package, "run", return_value='{"issues":["bad signature"]}'):
                with self.assertRaisesRegex(RuntimeError, "not accepted: Invalid"):
                    macos_package.notarize(Path("release.dmg"), [], logs)
            self.assertEqual(
                json.loads((logs / "release.dmg.notary.json").read_text(encoding="utf-8"))["id"],
                "rejected-id",
            )
            self.assertTrue((logs / "release.dmg.notary-log.json").exists())

    def test_timeout_does_not_count_as_success_or_lose_submission_id(self):
        result = subprocess.CompletedProcess(
            [], 69, '{"id":"pending-id","status":"In Progress"}', ""
        )
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(macos_package.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(RuntimeError, "not accepted: In Progress"):
                    macos_package.notarize(Path("release.dmg"), [], Path(directory))
            self.assertIn(
                "pending-id",
                (Path(directory) / "release.dmg.notary.json").read_text(encoding="utf-8"),
            )

    def test_bundle_requires_single_target_architecture_and_portable_dylibs(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory) / "Liftgate.app"
            (app / "Contents/MacOS").mkdir(parents=True)
            (app / "Contents/Resources").mkdir(parents=True)
            for name in ("liftgate-desktop", "liftgate", "lg"):
                (app / "Contents/MacOS" / name).write_bytes(b"\xcf\xfa\xed\xfe")
            (app / "Contents/Resources/AppIcon.icns").write_bytes(b"icns")
            (app / "Contents/Info.plist").write_bytes(
                plistlib.dumps({"CFBundleExecutable": "liftgate-desktop"})
            )

            cli_bin = app / "Contents/MacOS/lg"

            def mismatch_arch(*args):
                if args[:2] == ("lipo", "-archs"):
                    return "x86_64" if args[-1] == cli_bin else "arm64"
                return ""

            with patch.object(macos_package, "run", side_effect=mismatch_arch):
                with self.assertRaisesRegex(RuntimeError, "Expected arm64"):
                    macos_package.audit_bundle(app, "arm64")

            def homebrew_dep(*args):
                if args[:2] == ("lipo", "-archs"):
                    return "arm64"
                if args[:2] == ("otool", "-L"):
                    return "\t/opt/homebrew/lib/libfoo.dylib (compatibility version 1.0.0)\n"
                return ""

            with patch.object(macos_package, "run", side_effect=homebrew_dep):
                with self.assertRaisesRegex(RuntimeError, "Nonportable dependency"):
                    macos_package.audit_bundle(app, "arm64")

    def test_existing_artifact_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "release.dmg"
            output.write_bytes(b"original release")
            ent = Path(directory) / "Release.entitlements"
            ent.write_bytes(plistlib.dumps({}))
            with patch.dict(os.environ, {}, clear=True):
                with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
                    macos_package.package(
                        argparse.Namespace(
                            output=output,
                            zip_output=None,
                            entitlements=ent,
                            require_notarization=False,
                        )
                    )
            self.assertEqual(output.read_bytes(), b"original release")

    def test_partial_or_required_signing_credentials_fail_closed(self):
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(macos_package.resolve_signing_mode(False), "adhoc")
            with self.assertRaisesRegex(RuntimeError, "MACOS_SIGNING_IDENTITY"):
                macos_package.resolve_signing_mode(True)

        with patch.dict(
            os.environ, {"MACOS_CERTIFICATE_P12_BASE64": "dGVzdA=="}, clear=True
        ):
            with self.assertRaisesRegex(RuntimeError, "MACOS_SIGNING_IDENTITY"):
                macos_package.resolve_signing_mode(False)


if __name__ == "__main__":
    unittest.main()
