"""Offline loopback tests for scripts/install.sh and scripts/install.ps1."""

import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import threading
import unittest
import zipfile


REPO_ROOT = Path(__file__).resolve().parents[2]
INSTALL_SH = REPO_ROOT / "scripts" / "install.sh"
INSTALL_PS1 = REPO_ROOT / "scripts" / "install.ps1"


class _ReleaseServer:
    """In-memory HTTP server serving `/releases/latest/download/*` and `/releases/download/<tag>/*`."""

    def __init__(self, files: dict[str, bytes]):
        self.files = files

        outer = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802
                payload = outer.files.get(self.path)
                if payload is None:
                    self.send_response(404)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, fmt, *args):  # noqa: ARG002
                return

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.httpd.server_address[1]
        self.base_url = f"http://127.0.0.1:{self.port}"
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=5)


def _build_posix_tarball(target: str, version: str = "0.2.0", include_lg: bool = True) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        entries = {
            f"liftgate-{target}/liftgate": f"#!/bin/sh\necho 'liftgate {version}'\n".encode("utf-8"),
        }
        if include_lg:
            entries[f"liftgate-{target}/lg"] = f"#!/bin/sh\necho 'lg {version}'\n".encode("utf-8")
        for rel_path, body in entries.items():
            info = tarfile.TarInfo(rel_path)
            info.size = len(body)
            info.mode = 0o755
            tar.addfile(info, io.BytesIO(body))
    return buf.getvalue()


def _build_windows_zip(target: str, include_lg: bool = True) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, mode="w", compression=zipfile.ZIP_DEFLATED) as zf:
        zf.writestr(f"liftgate-{target}/liftgate.exe", b"MZ-fake-liftgate")
        if include_lg:
            zf.writestr(f"liftgate-{target}/lg.exe", b"MZ-fake-lg")
    return buf.getvalue()


class InstallerStaticContractTests(unittest.TestCase):
    def test_install_sh_uses_strict_lf_line_endings(self):
        raw = INSTALL_SH.read_bytes()
        self.assertNotIn(b"\r", raw, "scripts/install.sh must use LF line endings for curl | sh")
        self.assertTrue(raw.startswith(b"#!/bin/sh\n"))

    def test_install_ps1_parses_without_syntax_errors_when_powershell_available(self):
        shell = shutil.which("pwsh") or shutil.which("powershell")
        if not shell:
            self.skipTest("pwsh/powershell not installed")
        check_cmd = (
            "$tokens = $null; $errors = $null; "
            f"[void][System.Management.Automation.Language.Parser]::ParseFile('{INSTALL_PS1}', [ref]$tokens, [ref]$errors); "
            "if ($errors.Count -gt 0) { $errors | ForEach-Object { Write-Error $_.ToString() }; exit 1 }"
        )
        res = subprocess.run(
            [shell, "-NoProfile", "-Command", check_cmd],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)


@unittest.skipIf(os.name == "nt", "POSIX sh runtime test runs on Unix")
class PosixInstallScriptTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.dest = self.root / "bin"

    def _serve_release(self, target: str, tag: str = "v0.2.0", tamper_sha: bool = False, include_lg: bool = True):
        archive_name = f"liftgate-{target}.tar.gz"
        tar_bytes = _build_posix_tarball(target, version=tag.lstrip("v"), include_lg=include_lg)
        digest = "0" * 64 if tamper_sha else hashlib.sha256(tar_bytes).hexdigest()
        sums = f"{digest}  {archive_name}\n".encode("utf-8")
        server = _ReleaseServer(
            {
                f"/releases/latest/download/{archive_name}": tar_bytes,
                "/releases/latest/download/SHA256SUMS": sums,
                f"/releases/download/{tag}/{archive_name}": tar_bytes,
                f"/releases/download/{tag}/SHA256SUMS": sums,
            }
        )
        self.addCleanup(server.close)
        return server

    def test_fresh_install_and_update_install_only_cli_binaries(self):
        target = "x86_64-unknown-linux-musl"
        server = self._serve_release(target, tag="v0.2.0")
        env = {
            **os.environ,
            "LIFTGATE_BASE_URL": server.base_url,
            "LIFTGATE_BIN_DIR": str(self.dest),
            "LIFTGATE_TARGET": target,
            "LIFTGATE_NO_MODIFY_PATH": "1",
        }
        for extra_args in ([], ["--version", "v0.2.0"]):
            res = subprocess.run(
                ["sh", str(INSTALL_SH), *extra_args],
                env=env,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

        self.assertEqual(sorted(p.name for p in self.dest.iterdir()), ["lg", "liftgate"])
        self.assertEqual(
            subprocess.check_output(
                [str(self.dest / "liftgate"), "--version"],
                text=True,
                encoding="utf-8",
                errors="replace",
            ).strip(),
            "liftgate 0.2.0",
        )
        self.assertEqual(
            subprocess.check_output(
                [str(self.dest / "lg"), "--version"],
                text=True,
                encoding="utf-8",
                errors="replace",
            ).strip(),
            "lg 0.2.0",
        )

    def test_sha256_mismatch_or_ui_tag_or_incomplete_archive_leaves_existing_install_untouched(self):
        target = "x86_64-unknown-linux-musl"
        self.dest.mkdir(parents=True)
        existing = self.dest / "liftgate"
        existing.write_text("#!/bin/sh\necho 'liftgate 0.1.8'\n", encoding="utf-8")
        existing.chmod(0o755)

        # 1. Tampered SHA256SUMS fails closed.
        bad_sha_server = self._serve_release(target, tamper_sha=True)
        res = subprocess.run(
            ["sh", str(INSTALL_SH)],
            env={
                **os.environ,
                "LIFTGATE_BASE_URL": bad_sha_server.base_url,
                "LIFTGATE_BIN_DIR": str(self.dest),
                "LIFTGATE_TARGET": target,
                "LIFTGATE_NO_MODIFY_PATH": "1",
            },
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("SHA-256 mismatch", res.stderr)

        # 2. Legacy `-ui` tag is rejected with guidance.
        res_ui = subprocess.run(
            ["sh", str(INSTALL_SH), "--version", "v0.2.0-ui"],
            env={
                **os.environ,
                "LIFTGATE_BASE_URL": bad_sha_server.base_url,
                "LIFTGATE_BIN_DIR": str(self.dest),
                "LIFTGATE_TARGET": target,
                "LIFTGATE_NO_MODIFY_PATH": "1",
            },
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertNotEqual(res_ui.returncode, 0)
        self.assertIn("omit '-ui'", res_ui.stderr)

        # 3. Incomplete archive missing `lg` does not clobber existing `liftgate`.
        incomplete_server = self._serve_release(target, include_lg=False)
        res_inc = subprocess.run(
            ["sh", str(INSTALL_SH)],
            env={
                **os.environ,
                "LIFTGATE_BASE_URL": incomplete_server.base_url,
                "LIFTGATE_BIN_DIR": str(self.dest),
                "LIFTGATE_TARGET": target,
                "LIFTGATE_NO_MODIFY_PATH": "1",
            },
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertNotEqual(res_inc.returncode, 0)
        self.assertIn("0.1.8", existing.read_text(encoding="utf-8"))

    def test_merged_usr_dpkg_symlink_and_bash_login_profile_are_handled(self):
        target = "x86_64-unknown-linux-musl"
        server = self._serve_release(target, tag="v0.2.0")

        # Simulate merged-/usr: real file at usr/bin/liftgate owned by dpkg, exposed via bin -> usr/bin.
        fake_usr_bin = self.root / "usr" / "bin"
        fake_usr_bin.mkdir(parents=True)
        packaged_bin = fake_usr_bin / "liftgate"
        packaged_bin.write_text("#!/bin/sh\necho 'liftgate 0.1.0-deb'\n", encoding="utf-8")
        packaged_bin.chmod(0o755)

        fake_bin_link = self.root / "bin"
        fake_bin_link.symlink_to(fake_usr_bin)

        # Mock `dpkg -S` doing literal path lookup (succeeds only for usr/bin/liftgate, fails for bin/liftgate).
        mock_tools = self.root / "mock-tools"
        mock_tools.mkdir()
        mock_dpkg = mock_tools / "dpkg"
        mock_dpkg.write_text(
            f"#!/bin/sh\nif [ \"$1\" = \"-S\" ] && [ \"$2\" = \"{packaged_bin.resolve()}\" ]; then\n"
            "  echo 'liftgate: /usr/bin/liftgate'\n  exit 0\nfi\nexit 1\n",
            encoding="utf-8",
        )
        mock_dpkg.chmod(0o755)

        fake_home = self.root / "home"
        fake_home.mkdir()
        # Pre-existing .bashrc without .bash_profile sourcing it.
        (fake_home / ".bashrc").write_text("# existing bashrc\n", encoding="utf-8")

        env = {
            **os.environ,
            "HOME": str(fake_home),
            "SHELL": "/bin/bash",
            "PATH": f"{mock_tools}:{fake_bin_link}:{os.environ.get('PATH', '')}",
            "LIFTGATE_BASE_URL": server.base_url,
            "LIFTGATE_TARGET": target,
        }
        res = subprocess.run(
            ["sh", str(INSTALL_SH)],
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

        # dpkg-owned usr/bin/liftgate must remain untouched; standalone CLI installed in ~/.local/bin.
        self.assertIn("0.1.0-deb", packaged_bin.read_text(encoding="utf-8"))
        local_bin = fake_home / ".local" / "bin"
        self.assertTrue((local_bin / "liftgate").is_file())
        self.assertTrue((local_bin / "lg").is_file())

        # Both .bashrc and login profile (.profile on Linux) must configure PATH idempotently.
        self.assertIn(str(local_bin), (fake_home / ".bashrc").read_text(encoding="utf-8"))
        self.assertIn(str(local_bin), (fake_home / ".profile").read_text(encoding="utf-8"))
        if shutil.which("bash"):
            login_path = subprocess.check_output(
                ["bash", "--login", "-c", "printf '%s' \"$PATH\""],
                env={"HOME": str(fake_home), "PATH": "/usr/bin:/bin"},
                text=True,
                encoding="utf-8",
                errors="replace",
            )
            entries = [p for p in login_path.split(":") if p == str(local_bin)]
            self.assertEqual(len(entries), 1, f"expected {local_bin} once in login PATH, got: {login_path}")


@unittest.skipUnless(
    os.name == "nt" and (shutil.which("pwsh") or shutil.which("powershell")),
    "Windows PowerShell installer test runs on Windows",
)
class WindowsPowerShellInstallScriptTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.dest = self.root / "bin"
        self.shell = shutil.which("pwsh") or shutil.which("powershell")

    def _serve_release(self, target: str, tag: str = "v0.2.0", tamper_sha: bool = False):
        archive_name = f"liftgate-{target}.zip"
        zip_bytes = _build_windows_zip(target)
        digest = "0" * 64 if tamper_sha else hashlib.sha256(zip_bytes).hexdigest()
        sums = f"{digest}  {archive_name}\n".encode("utf-8")
        server = _ReleaseServer(
            {
                f"/releases/latest/download/{archive_name}": zip_bytes,
                "/releases/latest/download/SHA256SUMS": sums,
                f"/releases/download/{tag}/{archive_name}": zip_bytes,
                f"/releases/download/{tag}/SHA256SUMS": sums,
            }
        )
        self.addCleanup(server.close)
        return server

    def test_powershell_file_and_iex_install_only_cli_without_polluting_session(self):
        target = "x86_64-pc-windows-msvc"
        server = self._serve_release(target)
        env = {
            **os.environ,
            "LIFTGATE_BASE_URL": server.base_url,
            "LIFTGATE_BIN_DIR": str(self.dest),
            "LIFTGATE_TARGET": target,
            "LIFTGATE_SKIP_SMOKE_TEST": "1",
            "LIFTGATE_NO_MODIFY_PATH": "1",
        }
        # 1. Direct -File invocation.
        res = subprocess.run(
            [self.shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(INSTALL_PS1), "-Version", "0.2.0", "-NoModifyPath"],
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertEqual(sorted(p.name for p in self.dest.iterdir()), ["lg.exe", "liftgate.exe"])

        # 2. `Get-Content -Raw ... | Invoke-Expression` (`irm | iex` contract) preserves caller variables & preferences.
        iex_cmd = (
            "$Version = 'caller-var'; "
            "$ProgressPreference = 'Continue'; "
            f"Get-Content -Raw -LiteralPath '{INSTALL_PS1}' | Invoke-Expression; "
            "Write-Output ('PREF=' + $ProgressPreference + ';VER=' + $Version)"
        )
        res_iex = subprocess.run(
            [self.shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", iex_cmd],
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertEqual(res_iex.returncode, 0, res_iex.stdout + res_iex.stderr)
        self.assertIn("PREF=Continue;VER=caller-var", res_iex.stdout)
        self.assertEqual(sorted(p.name for p in self.dest.iterdir()), ["lg.exe", "liftgate.exe"])

    def test_powershell_sha256_mismatch_and_ui_tag_fail_closed(self):
        target = "x86_64-pc-windows-msvc"
        self.dest.mkdir(parents=True)
        existing = self.dest / "liftgate.exe"
        existing.write_bytes(b"existing-binary")

        bad_server = self._serve_release(target, tamper_sha=True)
        env = {
            **os.environ,
            "LIFTGATE_BASE_URL": bad_server.base_url,
            "LIFTGATE_BIN_DIR": str(self.dest),
            "LIFTGATE_TARGET": target,
            "LIFTGATE_SKIP_SMOKE_TEST": "1",
            "LIFTGATE_NO_MODIFY_PATH": "1",
        }
        res = subprocess.run(
            [self.shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(INSTALL_PS1)],
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertNotEqual(res.returncode, 0)
        self.assertEqual(existing.read_bytes(), b"existing-binary")

        res_ui = subprocess.run(
            [self.shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(INSTALL_PS1), "-Version", "v0.2.0-ui"],
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        self.assertNotEqual(res_ui.returncode, 0)
        self.assertEqual(existing.read_bytes(), b"existing-binary")


if __name__ == "__main__":
    unittest.main()
