#!/bin/sh
# Liftgate CLI (`lg` + `liftgate`) installer for Linux, macOS, and FreeBSD.
#
# One-liner:
#   curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh
#
# Pass flags through sh:
#   curl -fsSL https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.sh | sh -s -- --version v0.2.0
#
# Downloads the prebuilt archive matching your OS + CPU from GitHub Releases,
# verifies SHA-256 fail-closed against SHA256SUMS, smoke-tests both `liftgate`
# and `lg`, atomically installs them into your bin directory, and adds that
# directory to your shell PATH if needed.
#
# Environment overrides:
#   LIFTGATE_VERSION         pin a release tag (e.g. v0.2.0); default: latest
#   LIFTGATE_BIN_DIR         install dir; default: existing install dir or $HOME/.local/bin
#   LIFTGATE_NO_MODIFY_PATH  set to 1 to skip modifying shell profile files
#   LIFTGATE_TARGET          override Rust target triple (advanced)
#   LIFTGATE_REPO            GitHub owner/repo; default: acking-you/liftgate
#   LIFTGATE_BASE_URL        release host override; default: https://github.com/$LIFTGATE_REPO
#
# Strictly POSIX sh; zero bashisms.
set -eu

TMP_DIR=""
STAGE_MAIN=""
STAGE_ALIAS=""

cleanup() {
  if [ -n "$STAGE_MAIN" ] && [ -f "$STAGE_MAIN" ]; then
    rm -f "$STAGE_MAIN"
  fi
  if [ -n "$STAGE_ALIAS" ] && [ -f "$STAGE_ALIAS" ]; then
    rm -f "$STAGE_ALIAS"
  fi
  if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
    rm -rf "$TMP_DIR"
  fi
}
trap cleanup EXIT INT TERM HUP

info() { printf '\033[1;34m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$1" >&2; }
err() { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

usage() {
  cat <<'EOF'
Liftgate CLI installer (`lg` + `liftgate`)

Usage:
  install.sh [options]

Options:
  -v, --version <TAG>    Install a specific release tag (e.g. v0.2.0)
  -b, --bin-dir <DIR>    Install directory (default: $HOME/.local/bin)
      --no-modify-path   Do not append PATH export to shell profile
  -h, --help             Print this help message
EOF
}

VERSION="${LIFTGATE_VERSION:-}"
BIN_DIR="${LIFTGATE_BIN_DIR:-}"
NO_MODIFY_PATH="${LIFTGATE_NO_MODIFY_PATH:-0}"
REPO="${LIFTGATE_REPO:-acking-you/liftgate}"
TARGET_OVERRIDE="${LIFTGATE_TARGET:-}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    -v | --version)
      [ "$#" -ge 2 ] || err "missing value for $1"
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#*=}"
      shift
      ;;
    -b | --bin-dir)
      [ "$#" -ge 2 ] || err "missing value for $1"
      BIN_DIR="$2"
      shift 2
      ;;
    --bin-dir=*)
      BIN_DIR="${1#*=}"
      shift
      ;;
    --no-modify-path)
      NO_MODIFY_PATH="1"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      err "unknown option: $1 (run with --help for usage)"
      ;;
  esac
done

# --- validate repo, base URL, and optional version tag ----------------------
printf '%s' "$REPO" | grep -Eq '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$' ||
  err "invalid LIFTGATE_REPO: $REPO"

BASE_URL="${LIFTGATE_BASE_URL:-https://github.com/${REPO}}"
BASE_URL="$(printf '%s' "$BASE_URL" | sed 's#//*$##')"
case "$BASE_URL" in
  https://* | http://127.0.0.1:* | http://localhost:* | http://\[::1\]:*) ;;
  *) err "LIFTGATE_BASE_URL must use https:// (got: $BASE_URL)" ;;
esac

TAG=""
if [ -n "$VERSION" ]; then
  case "$VERSION" in
    *-ui)
      err "'$VERSION' is a Liftgate Desktop GUI tag; omit '-ui' for the CLI or download desktop installers from ${BASE_URL}/releases/tag/${VERSION}"
      ;;
    v* | V*)
      TAG="v${VERSION#?}"
      ;;
    *)
      TAG="v${VERSION}"
      ;;
  esac
  printf '%s' "$TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' ||
    err "invalid version tag '$VERSION' (expected e.g. v0.2.0)"
fi

# --- check required tools ---------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  download() { curl -fsSL --retry 3 --retry-delay 1 "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  download() { wget -qO "$2" "$1"; }
else
  err "need curl or wget to download Liftgate; please install one first."
fi

command -v tar >/dev/null 2>&1 || err "need tar to unpack the release archive."

compute_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print tolower($1)}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print tolower($1)}'
  elif command -v sha256 >/dev/null 2>&1; then
    sha256 -q "$1" | tr 'A-F' 'a-f'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | awk '{print tolower($NF)}'
  else
    err "need sha256sum, shasum, sha256, or openssl to verify SHA256SUMS."
  fi
}

# --- detect OS + CPU -> Rust target triple ----------------------------------
os="$(uname -s)"
arch="$(uname -m)"

detect_host_target() {
  case "$os" in
    Linux)
      case "$arch" in
        x86_64 | amd64) printf 'x86_64-unknown-linux-musl' ;;
        aarch64 | arm64) printf 'aarch64-unknown-linux-musl' ;;
        armv7l | armv8l | armhf) printf 'armv7-unknown-linux-musleabihf' ;;
        i686 | i386 | x86) printf 'i686-unknown-linux-musl' ;;
        riscv64) printf 'riscv64gc-unknown-linux-musl' ;;
        ppc64le) printf 'powerpc64le-unknown-linux-gnu' ;;
        s390x) printf 's390x-unknown-linux-gnu' ;;
        *) err "unsupported Linux CPU architecture: $arch" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64 | aarch64)
          printf 'aarch64-apple-darwin'
          ;;
        x86_64)
          # Prefer native Apple Silicon binary even when invoked inside Rosetta 2.
          if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || printf '0')" = "1" ]; then
            printf 'aarch64-apple-darwin'
          else
            printf 'x86_64-apple-darwin'
          fi
          ;;
        *) err "unsupported macOS CPU architecture: $arch" ;;
      esac
      ;;
    FreeBSD)
      case "$arch" in
        x86_64 | amd64) printf 'x86_64-unknown-freebsd' ;;
        *) err "unsupported FreeBSD CPU architecture: $arch" ;;
      esac
      ;;
    MINGW* | MSYS* | CYGWIN*)
      err "Windows shell detected; run in PowerShell instead: irm https://raw.githubusercontent.com/${REPO}/main/scripts/install.ps1 | iex"
      ;;
    *)
      err "unsupported OS: $os (supported: Linux, macOS, FreeBSD; use scripts/install.ps1 on Windows)"
      ;;
  esac
}

host_target=""
if [ -n "$TARGET_OVERRIDE" ]; then
  printf '%s' "$TARGET_OVERRIDE" | grep -Eq '^[A-Za-z0-9_-]+$' ||
    err "invalid LIFTGATE_TARGET: $TARGET_OVERRIDE"
  target="$TARGET_OVERRIDE"
else
  host_target="$(detect_host_target)"
  target="$host_target"
fi

archive_name="liftgate-${target}.tar.gz"
if [ -n "$TAG" ]; then
  release_label="$TAG"
  download_base="${BASE_URL}/releases/download/${TAG}"
else
  release_label="latest"
  download_base="${BASE_URL}/releases/latest/download"
fi

archive_url="${download_base}/${archive_name}"
checksums_url="${download_base}/SHA256SUMS"

# --- locate any existing standalone CLI installation ------------------------
check_single_packaged_path() {
  case "$1" in
    *.app/Contents/MacOS/* | /tmp/.mount_*) return 0 ;;
  esac
  if command -v dpkg >/dev/null 2>&1 && dpkg -S "$1" >/dev/null 2>&1; then
    return 0
  fi
  if command -v rpm >/dev/null 2>&1 && rpm -qf "$1" >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

is_packaged_bundle_path() {
  check_single_packaged_path "$1" && return 0

  # Resolve directory symlinks (e.g. merged-/usr `/bin -> /usr/bin` on Debian/Ubuntu,
  # where `dpkg -S` performs a literal path lookup against `/usr/bin/liftgate`).
  cand_dir_phys="$(cd -P "$(dirname "$1")" 2>/dev/null && pwd -P || true)"
  if [ -n "$cand_dir_phys" ]; then
    cand_phys="${cand_dir_phys}/$(basename "$1")"
    if [ "$cand_phys" != "$1" ] && check_single_packaged_path "$cand_phys"; then
      return 0
    fi
  fi

  # Also resolve leaf symlinks when `realpath` or `readlink -f` is available.
  cand_real=""
  if command -v realpath >/dev/null 2>&1; then
    cand_real="$(realpath "$1" 2>/dev/null || true)"
  elif command -v readlink >/dev/null 2>&1; then
    cand_real="$(readlink -f "$1" 2>/dev/null || true)"
  fi
  if [ -n "$cand_real" ] && [ "$cand_real" != "$1" ] && check_single_packaged_path "$cand_real"; then
    return 0
  fi

  return 1
}

find_existing_cli() {
  for candidate_name in liftgate lg; do
    cand="$(command -v "$candidate_name" 2>/dev/null || true)"
    case "$cand" in
      /*)
        if [ -f "$cand" ] && ! is_packaged_bundle_path "$cand"; then
          ver_out="$("$cand" --version </dev/null 2>/dev/null | awk 'NR==1{print}' || true)"
          case "$ver_out" in
            liftgate\ * | lg\ *)
              printf '%s\n' "$cand"
              return 0
              ;;
          esac
        fi
        ;;
    esac
  done
  return 1
}

# --- resolve destination directory before downloading -----------------------
existing=""
if [ -n "$BIN_DIR" ]; then
  mkdir -p "$BIN_DIR" || err "could not create install dir: $BIN_DIR"
  bindir="$(cd "$BIN_DIR" && pwd)"
  if [ -f "$bindir/liftgate" ] && [ ! -L "$bindir/liftgate" ]; then
    existing="$bindir/liftgate"
  fi
else
  detected="$(find_existing_cli || true)"
  if [ -n "$detected" ] && [ -w "$(dirname "$detected")" ]; then
    existing="$detected"
    bindir="$(cd "$(dirname "$detected")" && pwd)"
  else
    if [ -n "$detected" ]; then
      warn "existing install directory '$(dirname "$detected")' is not writable; installing standalone CLI into \$HOME/.local/bin instead."
    fi
    [ -n "${HOME:-}" ] || err "HOME is not set; specify --bin-dir <DIR>"
    mkdir -p "$HOME/.local/bin" || err "could not create $HOME/.local/bin"
    bindir="$(cd "$HOME/.local/bin" && pwd)"
    if [ -f "$bindir/liftgate" ] && [ ! -L "$bindir/liftgate" ]; then
      existing="$bindir/liftgate"
    fi
  fi
fi

if [ -n "$existing" ]; then
  cur_ver="$("$existing" --version </dev/null 2>/dev/null | awk 'NR==1{print $NF}' || true)"
  info "found Liftgate ${cur_ver:-(unknown)} at $existing — updating (${release_label} / ${target})"
else
  info "installing Liftgate CLI (${release_label} / ${target})"
fi

[ -w "$bindir" ] ||
  err "install directory '$bindir' is not writable (try --bin-dir \"\$HOME/.local/bin\")."
[ ! -d "$bindir/liftgate" ] || err "'$bindir/liftgate' is a directory; refusing to overwrite."
[ ! -d "$bindir/lg" ] || err "'$bindir/lg' is a directory; refusing to overwrite."

# --- download & fail-closed SHA-256 verification ----------------------------
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/liftgate-install.XXXXXX")"

info "downloading ${archive_url}"
download "$archive_url" "$TMP_DIR/$archive_name" ||
  err "failed to download ${archive_url}"

info "verifying SHA-256 checksum against SHA256SUMS"
download "$checksums_url" "$TMP_DIR/SHA256SUMS" ||
  err "failed to download ${checksums_url}"

expected_sha="$(
  awk -v file="$archive_name" '
    NF >= 2 {
      name = $2
      sub(/^\*/, "", name)
      if (name == file) {
        print tolower($1)
        exit
      }
    }
  ' "$TMP_DIR/SHA256SUMS"
)"
printf '%s' "$expected_sha" | grep -Eq '^[0-9a-f]{64}$' ||
  err "SHA256SUMS did not contain a valid SHA-256 entry for ${archive_name}"

actual_sha="$(compute_sha256 "$TMP_DIR/$archive_name")"
if [ "$actual_sha" != "$expected_sha" ]; then
  err "SHA-256 mismatch for ${archive_name}: expected ${expected_sha}, got ${actual_sha}"
fi

# --- unpack & smoke-test CLI binaries (`liftgate` + `lg`) -------------------
mkdir -p "$TMP_DIR/extract"
tar -xzf "$TMP_DIR/$archive_name" -C "$TMP_DIR/extract" ||
  err "failed to extract ${archive_name}"

pkg_dir="$TMP_DIR/extract/liftgate-${target}"
src_main="$pkg_dir/liftgate"
src_alias="$pkg_dir/lg"

[ -f "$src_main" ] && [ ! -L "$src_main" ] ||
  err "archive ${archive_name} is missing regular binary 'liftgate'"
[ -f "$src_alias" ] && [ ! -L "$src_alias" ] ||
  err "archive ${archive_name} is missing regular binary 'lg'"

chmod 0755 "$src_main" "$src_alias"

if [ "${LIFTGATE_SKIP_SMOKE_TEST:-0}" != "1" ] && { [ -z "$TARGET_OVERRIDE" ] || [ "$TARGET_OVERRIDE" = "${host_target:-}" ]; }; then
  "$src_main" --version </dev/null >/dev/null 2>&1 ||
    err "downloaded 'liftgate' binary failed smoke test (--version)"
  "$src_alias" --version </dev/null >/dev/null 2>&1 ||
    err "downloaded 'lg' binary failed smoke test (--version)"
fi

# --- atomic same-filesystem install (safe while background daemon runs) -----
STAGE_MAIN="$bindir/.liftgate.install.$$"
STAGE_ALIAS="$bindir/.lg.install.$$"

cp "$src_main" "$STAGE_MAIN" || err "failed to stage liftgate into $bindir"
cp "$src_alias" "$STAGE_ALIAS" || err "failed to stage lg into $bindir"
chmod 0755 "$STAGE_MAIN" "$STAGE_ALIAS"

mv -f "$STAGE_MAIN" "$bindir/liftgate" || err "failed to replace $bindir/liftgate"
STAGE_MAIN=""
mv -f "$STAGE_ALIAS" "$bindir/lg" || err "failed to replace $bindir/lg"
STAGE_ALIAS=""

installed_ver="$("$bindir/liftgate" --version </dev/null 2>/dev/null | awk 'NR==1{print $NF}' || printf '%s' "$release_label")"
info "installed liftgate & lg ${installed_ver} -> ${bindir}"

# --- ensure bin dir is on PATH ----------------------------------------------
ensure_profile_line() {
  target_profile="$1"
  target_dir="$2"
  export_line="$3"

  mkdir -p "$(dirname "$target_profile")"
  if [ -f "$target_profile" ] && grep -Fq "$target_dir" "$target_profile" 2>/dev/null; then
    return 0
  fi
  printf '\n# added by Liftgate installer\n%s\n' "$export_line" >>"$target_profile"
  info "added $target_dir to PATH in $target_profile"
}

add_to_path() {
  dir="$1"
  case ":${PATH:-}:" in
    *":$dir:"*) return 0 ;;
  esac

  if [ "$NO_MODIFY_PATH" = "1" ]; then
    warn "$dir is not on PATH (LIFTGATE_NO_MODIFY_PATH=1 — skipping profile update)."
    printf '  Add it manually:  export PATH="%s:$PATH"\n' "$dir"
    return 0
  fi

  case "$dir" in
    *:* | *'"'* | *"'"* | *'$'* | *'`'* | *'\'*) 
      warn "$dir contains special characters; skipping automatic shell profile edit."
      printf '  Add it manually to your shell PATH:  %s\n' "$dir"
      return 0
      ;;
  esac

  posix_line="case \":\$PATH:\" in *\":$dir:\"*) ;; *) export PATH=\"$dir:\$PATH\" ;; esac"
  shell_name="$(basename "${SHELL:-/bin/sh}")"
  case "$shell_name" in
    fish)
      ensure_profile_line "${HOME}/.config/fish/config.fish" "$dir" "fish_add_path \"$dir\""
      ;;
    zsh)
      ensure_profile_line "${ZDOTDIR:-$HOME}/.zshrc" "$dir" "$posix_line"
      ;;
    bash)
      ensure_profile_line "$HOME/.bashrc" "$dir" "$posix_line"
      if [ -f "$HOME/.bash_profile" ]; then
        login_profile="$HOME/.bash_profile"
      elif [ -f "$HOME/.bash_login" ]; then
        login_profile="$HOME/.bash_login"
      elif [ -f "$HOME/.profile" ]; then
        login_profile="$HOME/.profile"
      elif [ "$os" = "Darwin" ]; then
        login_profile="$HOME/.bash_profile"
      else
        login_profile="$HOME/.profile"
      fi
      ensure_profile_line "$login_profile" "$dir" "$posix_line"
      ;;
    *)
      ensure_profile_line "$HOME/.profile" "$dir" "$posix_line"
      ;;
  esac

  warn "open a new terminal, or run this in your current shell:"
  printf '    export PATH="%s:$PATH"\n' "$dir"
}

add_to_path "$bindir"

resolved_after="$(command -v liftgate 2>/dev/null || true)"
if [ -n "$resolved_after" ] && [ "$resolved_after" != "$bindir/liftgate" ]; then
  warn "'$resolved_after' appears earlier on PATH than '$bindir/liftgate'; place '$bindir' first on PATH if you want this CLI install to take precedence."
fi

if [ -n "$existing" ]; then
  info "done. Run 'lg r' if a background gateway is running, or 'lg --help' to explore."
else
  info "done. Next: run 'lg agy install && lg agy login', then 'lg cx' or 'lg cc'."
fi
