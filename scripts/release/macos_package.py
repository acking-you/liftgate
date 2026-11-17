#!/usr/bin/env python3
"""Assemble, sign, optionally notarize/staple, and verify Liftgate.app on macOS.

Follows the same credential contract and verification steps as pb-mapper:
  - When Apple Developer ID + Notary credentials are configured (or
    --require-notarization is passed), performs hardened-runtime signing,
    notarytool submission, ticket stapling, Gatekeeper spctl assessment, and
    mounted DMG verification. Partial signing env vars fail closed.
  - When no signing credentials are configured (and --require-notarization is
    not passed), performs ad-hoc codesign + strict bundle/Mach-O/mounted-DMG
    verification so unsigned CI environments still ship structurally verified
    DMG and ZIP bundles.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import secrets
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

APP_BUNDLE_NAME = "Liftgate.app"
VOLUME_NAME = "Liftgate"
REQUIRED_BINARIES = ("liftgate-desktop", "liftgate", "lg")

SIGNING_ENV_KEYS = (
    "MACOS_CERTIFICATE_P12_BASE64",
    "MACOS_CERTIFICATE_PASSWORD",
    "APPLE_NOTARY_PRIVATE_KEY",
    "MACOS_SIGNING_IDENTITY",
    "APPLE_TEAM_ID",
    "APPLE_NOTARY_KEY_ID",
    "APPLE_NOTARY_ISSUER_ID",
    "APPLE_NOTARY_PROFILE",
)


def run(*args: object) -> str:
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True)
    if result.returncode:
        # Never include command arguments: security/notarytool receive credentials.
        raise RuntimeError(f"{Path(str(args[0])).name} failed:\n{result.stdout}\n{result.stderr}")
    return result.stdout + result.stderr


def stapler(action: str, path: Path) -> str:
    # Even validation contacts Apple's ticket service. Retry transient URL
    # transport errors, never a rejected signature or invalid/missing ticket.
    for delay in (5, 15, 45, None):
        try:
            return run("xcrun", "stapler", action, path)
        except RuntimeError as error:
            if delay is None or not re.search(
                r"NSURLErrorDomain Code=-(?:1001|1003|1004|1005|1006|1009)\b",
                str(error),
            ):
                raise
            print(
                f"Apple ticket service network failure; retrying {action} in {delay}s...",
                flush=True,
            )
            time.sleep(delay)
    raise RuntimeError("Unreachable stapler retry exhaustion")


def required(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def resolve_signing_mode(require_notarization: bool) -> str:
    configured = [key for key in SIGNING_ENV_KEYS if os.environ.get(key)]
    if not configured and not require_notarization:
        return "adhoc"
    # Fail closed if signing was requested or partially configured.
    required("MACOS_SIGNING_IDENTITY")
    required("APPLE_TEAM_ID")
    if os.environ.get("MACOS_CERTIFICATE_P12_BASE64"):
        required("MACOS_CERTIFICATE_PASSWORD")
        required("APPLE_NOTARY_PRIVATE_KEY")
        required("APPLE_NOTARY_KEY_ID")
        required("APPLE_NOTARY_ISSUER_ID")
    else:
        required("APPLE_NOTARY_PROFILE")
    return "developer_id"


@contextlib.contextmanager
def credentials(work: Path):
    """CI uses a disposable keychain; local callers may use an existing profile."""
    if os.environ.get("MACOS_CERTIFICATE_P12_BASE64"):
        password = required("MACOS_CERTIFICATE_PASSWORD")
        p12 = work / "identity.p12"
        p12.write_bytes(base64.b64decode(required("MACOS_CERTIFICATE_P12_BASE64"), validate=True))
        p12.chmod(0o600)
        keychain = work / "signing.keychain-db"
        keychain_password = secrets.token_urlsafe(32)
        search_list = shlex.split(run("security", "list-keychains", "-d", "user"))
        run("security", "create-keychain", "-p", keychain_password, keychain)
        try:
            run("security", "set-keychain-settings", "-lut", "21600", keychain)
            run("security", "unlock-keychain", "-p", keychain_password, keychain)
            run(
                "security",
                "import",
                p12,
                "-k",
                keychain,
                "-P",
                password,
                "-T",
                "/usr/bin/codesign",
            )
            run(
                "security",
                "set-key-partition-list",
                "-S",
                "apple-tool:,apple:,codesign:",
                "-s",
                "-k",
                keychain_password,
                keychain,
            )
            run("security", "list-keychains", "-d", "user", "-s", *search_list, keychain)
            identities = run("security", "find-identity", "-v", "-p", "codesigning", keychain)
            if required("MACOS_SIGNING_IDENTITY") not in identities:
                raise RuntimeError(
                    "The configured Developer ID identity is not valid in the imported keychain"
                )
            p12.unlink()
            key = work / "AuthKey.p8"
            key.write_text(required("APPLE_NOTARY_PRIVATE_KEY"), encoding="utf-8")
            key.chmod(0o600)
            profile = "liftgate-release"
            run(
                "xcrun",
                "notarytool",
                "store-credentials",
                profile,
                "--key",
                key,
                "--key-id",
                required("APPLE_NOTARY_KEY_ID"),
                "--issuer",
                required("APPLE_NOTARY_ISSUER_ID"),
                "--keychain",
                keychain,
            )
            key.unlink()
            yield ["--keychain", str(keychain)], [
                "--keychain-profile",
                profile,
                "--keychain",
                str(keychain),
            ]
        finally:
            try:
                run("security", "list-keychains", "-d", "user", "-s", *search_list)
            finally:
                run("security", "delete-keychain", keychain)
    else:
        profile = required("APPLE_NOTARY_PROFILE")
        keychain = os.environ.get("MACOS_SIGNING_KEYCHAIN")
        signing = ["--keychain", keychain] if keychain else []
        notary_keychain = os.environ.get("APPLE_NOTARY_KEYCHAIN")
        notary = ["--keychain-profile", profile]
        if notary_keychain:
            notary += ["--keychain", notary_keychain]
        yield signing, notary


def macho_files(app: Path) -> list[Path]:
    magics = {
        bytes.fromhex(x)
        for x in (
            "cffaedfe",
            "feedfacf",
            "cefaedfe",
            "feedface",
            "cafebabe",
            "bebafeca",
            "cafebabf",
            "bfbafeca",
        )
    }
    files: list[Path] = []
    for path in app.rglob("*"):
        if path.is_file() and not path.is_symlink():
            with path.open("rb") as handle:
                if handle.read(4) in magics:
                    files.append(path)
    return sorted(files)


def audit_bundle(app: Path, arch: str) -> tuple[list[Path], dict]:
    info_path = app / "Contents/Info.plist"
    if not info_path.is_file():
        raise RuntimeError(f"Missing Info.plist: {info_path}")
    info = plistlib.loads(info_path.read_bytes())
    executable = info["CFBundleExecutable"]
    expected_machos = {app / "Contents/MacOS" / name for name in (executable, *REQUIRED_BINARIES)}
    for component in sorted(expected_machos):
        if not component.is_file():
            raise RuntimeError(f"Missing release component: {component.relative_to(app)}")
    icon_file = info.get("CFBundleIconFile", "AppIcon")
    if not icon_file.endswith(".icns"):
        icon_file = f"{icon_file}.icns"
    if not (app / "Contents/Resources" / icon_file).is_file():
        raise RuntimeError(f"Missing bundle icon: Contents/Resources/{icon_file}")

    files = macho_files(app)
    minimum = tuple(int(part) for part in info.get("LSMinimumSystemVersion", "11.0").split("."))
    for path in files:
        arches = set(run("lipo", "-archs", path).split())
        if arch not in arches or (path in expected_machos and arches != {arch}):
            raise RuntimeError(f"Expected {arch} in {path.name}, got {arches}")
        for line in run("otool", "-L", path).splitlines():
            if line.startswith("\t"):
                dependency = line.strip().split(" (", 1)[0]
                if not dependency.startswith(
                    (
                        "@rpath/",
                        "@loader_path/",
                        "@executable_path/",
                        "/usr/lib/",
                        "/System/Library/",
                    )
                ):
                    raise RuntimeError(f"Nonportable dependency in {path.name}: {dependency}")
        for command in run("otool", "-l", path).split("Load command "):
            if re.search(r"\bcmd LC_(?:BUILD_VERSION|VERSION_MIN_MACOSX)\b", command):
                version = re.search(
                    r"^\s*(?:minos|version) (\d+(?:\.\d+)+)$", command, re.MULTILINE
                )
                if version:
                    minimum = max(minimum, tuple(int(p) for p in version[1].split(".")))

    info["LSMinimumSystemVersion"] = ".".join(str(p) for p in minimum)
    info_path.write_bytes(plistlib.dumps(info))
    return files, info


def build_icns(icon_png: Path, output_icns: Path, work: Path) -> None:
    iconset = work / "AppIcon.iconset"
    iconset.mkdir(parents=True, exist_ok=True)
    sizes = (
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    )
    for pixels, filename in sizes:
        run("sips", "-z", pixels, pixels, icon_png, "--out", iconset / filename)
    output_icns.parent.mkdir(parents=True, exist_ok=True)
    run("iconutil", "-c", "icns", iconset, "-o", output_icns)


def assemble(args: argparse.Namespace) -> Path:
    app_out = args.output.resolve()
    if app_out.exists():
        shutil.rmtree(app_out)
    macos_dir = app_out / "Contents/MacOS"
    resources_dir = app_out / "Contents/Resources"
    macos_dir.mkdir(parents=True)
    resources_dir.mkdir(parents=True)

    bin_dir = args.bin_dir.resolve()
    for name in REQUIRED_BINARIES:
        src = bin_dir / name
        if not src.is_file():
            raise RuntimeError(f"Missing compiled binary: {src}")
        dst = macos_dir / name
        shutil.copy2(src, dst)
        dst.chmod(0o755)

    clean_version = args.version.removeprefix("v")
    info = plistlib.loads(args.info_plist.read_bytes())
    info["CFBundleShortVersionString"] = clean_version
    info["CFBundleVersion"] = clean_version
    (app_out / "Contents/Info.plist").write_bytes(plistlib.dumps(info))

    with tempfile.TemporaryDirectory(prefix="liftgate-iconset-") as temp_dir:
        build_icns(args.icon_png.resolve(), resources_dir / "AppIcon.icns", Path(temp_dir))

    return app_out


def notarize(path: Path, auth: list[str], log_dir: Path) -> None:
    print(f"Submitting {path.name} to Apple notarization...", flush=True)
    submission = subprocess.run(
        [
            "xcrun",
            "notarytool",
            "submit",
            str(path),
            *auth,
            "--wait",
            "--timeout",
            "2h",
            "--output-format",
            "json",
        ],
        capture_output=True,
        text=True,
    )
    try:
        result = json.loads(submission.stdout)
    except ValueError:
        raise RuntimeError(
            f"notarytool did not return a submission result: {submission.stderr}"
        ) from None
    (log_dir / f"{path.name}.notary.json").write_text(
        json.dumps(result, indent=2) + "\n", encoding="utf-8"
    )
    if submission.returncode or result.get("status") != "Accepted":
        if result.get("id") and result.get("status") == "Invalid":
            log = run("xcrun", "notarytool", "log", result["id"], *auth)
            (log_dir / f"{path.name}.notary-log.json").write_text(log, encoding="utf-8")
        raise RuntimeError(
            f"Apple notarization was not accepted: {result.get('status')}; see {log_dir}"
        )
    print(f"Apple accepted {path.name}: {result['id']}", flush=True)


def detach_volume(mount: Path) -> None:
    try:
        run("hdiutil", "detach", mount)
    except RuntimeError:
        time.sleep(2)
        run("hdiutil", "detach", "-force", mount)


def verify_mounted_app(
    image: Path,
    work: Path,
    expected_info: dict,
    team: str | None,
    logs: Path,
    *,
    adhoc: bool = False,
) -> None:
    """Validate the application users receive inside the mounted DMG."""
    mount = work / "mounted"
    mount.mkdir()
    run("hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", mount, image)
    try:
        app = mount / APP_BUNDLE_NAME
        info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
        for key in (
            "CFBundleIdentifier",
            "CFBundleExecutable",
            "CFBundleShortVersionString",
            "CFBundleVersion",
        ):
            if info.get(key) != expected_info.get(key):
                raise RuntimeError(f"Mounted release has a mismatched {key}")
        report = run("codesign", "--verify", "--deep", "--strict", "--verbose=2", app)
        signature = run("codesign", "--display", "--verbose=4", app)
        report += signature
        if not adhoc:
            if (
                not team
                or f"TeamIdentifier={team}" not in signature
                or "Authority=Developer ID Application:" not in signature
            ):
                raise RuntimeError(
                    "Mounted application is missing the expected Developer ID signature"
                )
            report += stapler("validate", app)
            report += run("spctl", "--assess", "--type", "execute", "--verbose=4", app)
            if shutil.which("syspolicy_check"):
                report += run("syspolicy_check", "distribution", app)
        (logs / "mounted-app-verification.txt").write_text(report, encoding="utf-8")
    finally:
        detach_volume(mount)


def package(args: argparse.Namespace) -> None:
    require_notarization = bool(getattr(args, "require_notarization", False))
    mode = resolve_signing_mode(require_notarization)
    output = args.output.resolve()
    if output.exists():
        raise RuntimeError(f"Refusing to overwrite existing artifact: {output}")
    zip_output = args.zip_output.resolve() if getattr(args, "zip_output", None) else None
    if zip_output is not None and zip_output.exists():
        raise RuntimeError(f"Refusing to overwrite existing artifact: {zip_output}")

    output.parent.mkdir(parents=True, exist_ok=True)
    if zip_output is not None:
        zip_output.parent.mkdir(parents=True, exist_ok=True)
    logs = output.parent / (output.stem + "-verification")
    logs.mkdir(exist_ok=True)

    entitlements_path = args.entitlements.resolve()
    entitlements = plistlib.loads(entitlements_path.read_bytes())
    if entitlements.get("com.apple.security.get-task-allow"):
        raise RuntimeError("Release entitlements must not allow debugger attachment")

    with tempfile.TemporaryDirectory(
        prefix="liftgate-signing-", dir=os.environ.get("RUNNER_TEMP")
    ) as temporary:
        work = Path(temporary)
        stage = work / "image"
        stage.mkdir()
        app = stage / APP_BUNDLE_NAME
        run("ditto", args.app.resolve(), app)
        files, info = audit_bundle(app, args.arch)
        main_exec = app / "Contents/MacOS" / info["CFBundleExecutable"]

        if mode == "developer_id":
            identity = required("MACOS_SIGNING_IDENTITY")
            team = required("APPLE_TEAM_ID")
            with credentials(work) as (signing, auth):
                def sign_dev(path: Path, app_entitlements: bool = False) -> None:
                    options = ["--entitlements", str(entitlements_path)] if app_entitlements else []
                    run(
                        "codesign",
                        "--force",
                        "--sign",
                        identity,
                        "--timestamp",
                        "--options",
                        "runtime",
                        *signing,
                        *options,
                        path,
                    )

                for path in files:
                    if path != main_exec:
                        sign_dev(path)
                bundles = [
                    p
                    for p in app.rglob("*")
                    if p.is_dir()
                    and not p.is_symlink()
                    and p.suffix in {".framework", ".app", ".xpc", ".appex"}
                ]
                for path in sorted(bundles, key=lambda p: len(p.parts), reverse=True):
                    sign_dev(path)
                sign_dev(app, app_entitlements=True)
                run("codesign", "--verify", "--deep", "--strict", "--verbose=2", app)
                for path in [*files, app]:
                    details = run("codesign", "--display", "--verbose=4", path)
                    if (
                        f"TeamIdentifier={team}" not in details
                        or "Authority=Developer ID Application:" not in details
                        or "Timestamp=" not in details
                        or "(runtime)" not in details
                    ):
                        raise RuntimeError(
                            f"Not a timestamped Developer ID signature for team {team}: {path.name}"
                        )

                archive = work / "liftgate-app-notary.zip"
                run("ditto", "-c", "-k", "--keepParent", app, archive)
                notarize(archive, auth, logs)
                stapler("staple", app)
                stapler("validate", app)
                run("spctl", "--assess", "--type", "execute", "--verbose=2", app)

                if zip_output is not None:
                    run("ditto", "-c", "-k", "--keepParent", app, zip_output)

                (stage / "Applications").symlink_to("/Applications")
                candidate = work / output.name
                run(
                    "hdiutil",
                    "create",
                    "-volname",
                    VOLUME_NAME,
                    "-srcfolder",
                    stage,
                    "-format",
                    "UDZO",
                    candidate,
                )
                run("codesign", "--sign", identity, "--timestamp", *signing, candidate)
                notarize(candidate, auth, logs)
                stapler("staple", candidate)
                stapler("validate", candidate)
                run("hdiutil", "verify", candidate)
                run("codesign", "--verify", "--strict", candidate)
                run(
                    "spctl",
                    "--assess",
                    "--type",
                    "open",
                    "--context",
                    "context:primary-signature",
                    "--verbose=2",
                    candidate,
                )
                verify_mounted_app(candidate, work, info, team, logs, adhoc=False)
                run("ditto", candidate, output)
        else:
            team = None
            for path in files:
                if path != main_exec:
                    run("codesign", "--force", "--sign", "-", path)
            run(
                "codesign",
                "--force",
                "--sign",
                "-",
                "--entitlements",
                entitlements_path,
                app,
            )
            run("codesign", "--verify", "--deep", "--strict", "--verbose=2", app)

            if zip_output is not None:
                run("ditto", "-c", "-k", "--keepParent", app, zip_output)

            (stage / "Applications").symlink_to("/Applications")
            candidate = work / output.name
            run(
                "hdiutil",
                "create",
                "-volname",
                VOLUME_NAME,
                "-srcfolder",
                stage,
                "-format",
                "UDZO",
                candidate,
            )
            run("codesign", "--force", "--sign", "-", candidate)
            run("hdiutil", "verify", candidate)
            verify_mounted_app(candidate, work, info, None, logs, adhoc=True)
            run("ditto", candidate, output)

        report = {
            "version": info["CFBundleShortVersionString"],
            "build": info["CFBundleVersion"],
            "minimum_macos": info["LSMinimumSystemVersion"],
            "signing_mode": mode,
            "team_id": team,
            "architectures": [args.arch],
            "signed_macho_files": len(files),
            "mounted_app_verified": True,
            "sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
            "size": output.stat().st_size,
        }
        if zip_output is not None:
            report["zip_sha256"] = hashlib.sha256(zip_output.read_bytes()).hexdigest()
            report["zip_size"] = zip_output.stat().st_size
        (logs / "release.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(report, indent=2))


def build_parser() -> argparse.ArgumentParser:
    default_entitlements = (
        Path(__file__).resolve().parents[2] / "packaging/macos/Release.entitlements"
    )
    default_plist = Path(__file__).resolve().parents[2] / "packaging/macos/Info.plist"
    default_icon = Path(__file__).resolve().parents[2] / "packaging/icons/app_icon.png"

    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command")

    assemble_parser = subparsers.add_parser("assemble", help="Assemble Liftgate.app bundle")
    assemble_parser.add_argument("--bin-dir", type=Path, required=True)
    assemble_parser.add_argument("--output", type=Path, required=True)
    assemble_parser.add_argument("--version", required=True)
    assemble_parser.add_argument("--info-plist", type=Path, default=default_plist)
    assemble_parser.add_argument("--icon-png", type=Path, default=default_icon)

    package_parser = subparsers.add_parser(
        "package", help="Sign, optionally notarize, and package DMG/ZIP"
    )
    package_parser.add_argument("--app", type=Path, required=True)
    package_parser.add_argument("--output", type=Path, required=True)
    package_parser.add_argument("--zip-output", type=Path)
    package_parser.add_argument("--arch", choices=("arm64", "x86_64"), required=True)
    package_parser.add_argument("--entitlements", type=Path, default=default_entitlements)
    package_parser.add_argument("--require-notarization", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> None:
    raw_argv = list(sys.argv[1:] if argv is None else argv)
    if raw_argv and raw_argv[0] not in {"assemble", "package", "-h", "--help"}:
        raw_argv = ["package", *raw_argv]
    parser = build_parser()
    args = parser.parse_args(raw_argv)
    if args.command == "assemble":
        print(assemble(args))
    elif args.command == "package":
        package(args)
    else:
        parser.print_help()
        raise SystemExit(2)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError) as error:
        message = str(error)
        for name in (
            "MACOS_CERTIFICATE_PASSWORD",
            "MACOS_CERTIFICATE_P12_BASE64",
            "APPLE_NOTARY_PRIVATE_KEY",
        ):
            value = os.environ.get(name)
            if value:
                message = message.replace(value, "[redacted]")
        raise SystemExit(message) from None
