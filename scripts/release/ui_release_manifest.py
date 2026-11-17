#!/usr/bin/env python3
"""Write, sign, or verify the Liftgate Desktop release manifest and SHA256SUMS.

    build:  ui_release_manifest.py build <dir> <tag> <commit> [--cargo-toml Cargo.toml]
    sign:   ui_release_manifest.py sign <manifest> --key private.pem
    verify: ui_release_manifest.py verify <manifest> --public-key public.pem

`build` refuses a release that is missing any expected desktop package, contains
unexpected or empty files, or whose tag version does not match `[workspace.package]`
in `Cargo.toml`.

Signing uses OpenSSL 3 (`pkeyutl -rawin`) with a 64-byte Ed25519 signature
encoded as base64 in `release-manifest.json.sig`.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib

MANIFEST = "release-manifest.json"
CHECKSUMS = "SHA256SUMS"

EXPECTED = {
    "liftgate-desktop-windows-x86_64-setup.exe",
    "liftgate-desktop-windows-x86_64.zip",
    "liftgate-desktop-windows-arm64-setup.exe",
    "liftgate-desktop-windows-arm64.zip",
    "liftgate-desktop-macos-arm64.dmg",
    "liftgate-desktop-macos-arm64.zip",
    "liftgate-desktop-macos-x86_64.dmg",
    "liftgate-desktop-macos-x86_64.zip",
    "liftgate-desktop-linux-x86_64.AppImage",
    "liftgate-desktop-linux-amd64.deb",
    "liftgate-desktop-linux-x86_64.tar.gz",
}

DEFAULT_CARGO_TOML = Path(__file__).resolve().parents[2] / "Cargo.toml"


def digest(path: Path) -> str:
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def read_workspace_version(cargo_toml: Path) -> str:
    data = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
    version = data.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(version, str) or not version:
        raise SystemExit(f"{cargo_toml} does not declare [workspace.package].version")
    return version


def build(
    directory: Path,
    tag: str,
    commit: str,
    cargo_toml: Path = DEFAULT_CARGO_TOML,
) -> Path:
    match = re.fullmatch(r"v(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)", tag)
    if match is None or tag.endswith("-ui"):
        raise SystemExit(f"Tag {tag} is not vX.Y.Z")
    version = match[1]
    declared = read_workspace_version(cargo_toml)
    if declared != version:
        raise SystemExit(
            f"{cargo_toml} declares {declared}, the tag is {tag}: "
            "bump [workspace.package].version with the release"
        )

    ignored = {MANIFEST, f"{MANIFEST}.sig", CHECKSUMS}
    files = {
        p.name: p
        for p in directory.iterdir()
        if p.is_file() and p.name not in ignored
    }
    if set(files) != EXPECTED:
        missing = sorted(EXPECTED - set(files))
        extra = sorted(set(files) - EXPECTED)
        raise SystemExit(f"Release files do not match: missing {missing}, unexpected {extra}")

    assets = []
    checksum_lines = []
    for name in sorted(files):
        size = files[name].stat().st_size
        if size <= 0:
            raise SystemExit(f"Release file {name} is empty")
        sha256 = digest(files[name])
        assets.append({"name": name, "size": size, "sha256": sha256})
        checksum_lines.append(f"{sha256}  {name}")

    manifest = {
        "version": version,
        "tag": f"{tag}-ui",
        "commit": commit,
        "assets": assets,
    }
    output = directory / MANIFEST
    output.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    output.with_name(output.name + ".sig").unlink(missing_ok=True)
    (directory / CHECKSUMS).write_text("\n".join(checksum_lines) + "\n", encoding="utf-8")
    return output


def sign(manifest: Path, key: Path) -> Path:
    with tempfile.TemporaryDirectory() as work:
        raw = Path(work) / "signature"
        subprocess.run(
            [
                "openssl",
                "pkeyutl",
                "-sign",
                "-rawin",
                "-inkey",
                str(key),
                "-in",
                str(manifest),
                "-out",
                str(raw),
            ],
            check=True,
        )
        signature = raw.read_bytes()
    if len(signature) != 64:
        raise SystemExit("The key did not produce an Ed25519 signature")
    output = manifest.with_name(manifest.name + ".sig")
    output.write_text(base64.b64encode(signature).decode() + "\n", encoding="utf-8")
    return output


def verify(manifest: Path, public_key: Path) -> None:
    signature = manifest.with_name(manifest.name + ".sig")
    with tempfile.TemporaryDirectory() as work:
        raw = Path(work) / "signature"
        raw.write_bytes(base64.b64decode(signature.read_text(encoding="utf-8").strip()))
        subprocess.run(
            [
                "openssl",
                "pkeyutl",
                "-verify",
                "-rawin",
                "-pubin",
                "-inkey",
                str(public_key),
                "-in",
                str(manifest),
                "-sigfile",
                str(raw),
            ],
            check=True,
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    commands = parser.add_subparsers(dest="command", required=True)

    build_args = commands.add_parser("build")
    build_args.add_argument("directory", type=Path)
    build_args.add_argument("tag")
    build_args.add_argument("commit")
    build_args.add_argument("--cargo-toml", type=Path, default=DEFAULT_CARGO_TOML)

    sign_args = commands.add_parser("sign")
    sign_args.add_argument("manifest", type=Path)
    sign_args.add_argument("--key", type=Path, required=True)

    verify_args = commands.add_parser("verify")
    verify_args.add_argument("manifest", type=Path)
    verify_args.add_argument("--public-key", type=Path, required=True)

    parsed = parser.parse_args()
    if parsed.command == "build":
        print(build(parsed.directory, parsed.tag, parsed.commit, parsed.cargo_toml))
    elif parsed.command == "sign":
        print(sign(parsed.manifest, parsed.key))
    else:
        verify(parsed.manifest, parsed.public_key)
        print("Signature verified")
