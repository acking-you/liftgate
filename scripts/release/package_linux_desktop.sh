#!/usr/bin/env bash
# Package Liftgate Desktop on Linux x86_64 into .AppImage, .deb, and portable .tar.gz.
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <version> <bin-dir> <out-dir>" >&2
  exit 2
fi

RAW_VERSION="$1"
BIN_DIR="$2"
OUT_DIR="$3"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ICON_256="${REPO_ROOT}/packaging/icons/app_icon_256.png"
ICON_512="${REPO_ROOT}/packaging/icons/app_icon.png"
DESKTOP_SRC="${REPO_ROOT}/packaging/linux/liftgate-desktop.desktop"

for bin in liftgate-desktop liftgate lg; do
  if [ ! -x "${BIN_DIR}/${bin}" ]; then
    echo "::error::Missing executable ${BIN_DIR}/${bin}" >&2
    exit 1
  fi
done

for asset in "${ICON_256}" "${ICON_512}" "${DESKTOP_SRC}"; do
  if [ ! -f "${asset}" ]; then
    echo "::error::Missing packaging asset ${asset}" >&2
    exit 1
  fi
done

if command -v desktop-file-validate >/dev/null 2>&1; then
  desktop-file-validate "${DESKTOP_SRC}"
fi

mkdir -p "${OUT_DIR}"
OUT_ABS="$(cd "${OUT_DIR}" && pwd)"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/liftgate-linux-pkg.XXXXXX")"
trap 'rm -rf "${WORK_DIR}"' EXIT

# dpkg requires Version to begin with a digit.
DEB_VER="$(printf '%s' "${RAW_VERSION#v}" | sed 's/[^0-9A-Za-z.+~-]/./g')"
if ! printf '%s' "${DEB_VER}" | grep -qE '^[0-9]'; then
  DEB_VER="0.0.0+${DEB_VER}"
fi

# 1) Portable tarball (liftgate-desktop-linux-x86_64.tar.gz)
PORTABLE_NAME="liftgate-desktop-linux-x86_64"
PORTABLE_STAGE="${WORK_DIR}/${PORTABLE_NAME}"
mkdir -p "${PORTABLE_STAGE}"
cp "${BIN_DIR}/liftgate-desktop" "${BIN_DIR}/liftgate" "${BIN_DIR}/lg" "${PORTABLE_STAGE}/"
chmod 0755 "${PORTABLE_STAGE}/liftgate-desktop" "${PORTABLE_STAGE}/liftgate" "${PORTABLE_STAGE}/lg"
cp "${DESKTOP_SRC}" "${PORTABLE_STAGE}/liftgate-desktop.desktop"
cp "${ICON_512}" "${PORTABLE_STAGE}/liftgate-desktop.png"
cp "${REPO_ROOT}/README.md" "${REPO_ROOT}/README.zh-CN.md" "${REPO_ROOT}/CHANGELOG.md" \
   "${REPO_ROOT}/LICENSE-MIT" "${REPO_ROOT}/LICENSE-APACHE" "${PORTABLE_STAGE}/"
tar -czf "${OUT_ABS}/${PORTABLE_NAME}.tar.gz" -C "${WORK_DIR}" "${PORTABLE_NAME}"

# 2) AppImage (liftgate-desktop-linux-x86_64.AppImage)
APPDIR="${WORK_DIR}/AppDir"
mkdir -p "${APPDIR}/usr/bin" \
         "${APPDIR}/usr/share/applications" \
         "${APPDIR}/usr/share/icons/hicolor/256x256/apps" \
         "${APPDIR}/usr/share/icons/hicolor/512x512/apps" \
         "${APPDIR}/usr/share/doc/liftgate-desktop"

cp "${BIN_DIR}/liftgate-desktop" "${BIN_DIR}/liftgate" "${BIN_DIR}/lg" "${APPDIR}/usr/bin/"
chmod 0755 "${APPDIR}/usr/bin/liftgate-desktop" "${APPDIR}/usr/bin/liftgate" "${APPDIR}/usr/bin/lg"
cp "${DESKTOP_SRC}" "${APPDIR}/usr/share/applications/liftgate-desktop.desktop"
cp "${DESKTOP_SRC}" "${APPDIR}/liftgate-desktop.desktop"
cp "${ICON_256}" "${APPDIR}/usr/share/icons/hicolor/256x256/apps/liftgate-desktop.png"
cp "${ICON_512}" "${APPDIR}/usr/share/icons/hicolor/512x512/apps/liftgate-desktop.png"
cp "${ICON_256}" "${APPDIR}/liftgate-desktop.png"
ln -sf "liftgate-desktop.png" "${APPDIR}/.DirIcon"
cp "${REPO_ROOT}/LICENSE-MIT" "${REPO_ROOT}/LICENSE-APACHE" "${APPDIR}/usr/share/doc/liftgate-desktop/"

cat > "${APPDIR}/AppRun" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="${HERE}/usr/bin:${PATH:-}"
exec "${HERE}/usr/bin/liftgate-desktop" "$@"
EOF
chmod 0755 "${APPDIR}/AppRun"

APPIMAGETOOL="${WORK_DIR}/appimagetool-x86_64.AppImage"
curl -fsSL --retry 3 \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage" \
  -o "${APPIMAGETOOL}"
chmod 0755 "${APPIMAGETOOL}"

APPIMAGE_OUT="${OUT_ABS}/liftgate-desktop-linux-x86_64.AppImage"
ARCH=x86_64 "${APPIMAGETOOL}" --appimage-extract-and-run "${APPDIR}" "${APPIMAGE_OUT}"
chmod 0755 "${APPIMAGE_OUT}"

# Smoke-verify headless CLI version invocation inside the packaged AppImage.
"${APPIMAGE_OUT}" --appimage-extract-and-run --version

# 3) Debian package (liftgate-desktop-linux-amd64.deb)
DEB_ROOT="${WORK_DIR}/liftgate-deb"
mkdir -p "${DEB_ROOT}/DEBIAN" \
         "${DEB_ROOT}/usr/bin" \
         "${DEB_ROOT}/usr/share/applications" \
         "${DEB_ROOT}/usr/share/icons/hicolor/256x256/apps" \
         "${DEB_ROOT}/usr/share/icons/hicolor/512x512/apps" \
         "${DEB_ROOT}/usr/share/doc/liftgate-desktop"

cp "${BIN_DIR}/liftgate-desktop" "${BIN_DIR}/liftgate" "${BIN_DIR}/lg" "${DEB_ROOT}/usr/bin/"
chmod 0755 "${DEB_ROOT}/usr/bin/liftgate-desktop" "${DEB_ROOT}/usr/bin/liftgate" "${DEB_ROOT}/usr/bin/lg"
cp "${DESKTOP_SRC}" "${DEB_ROOT}/usr/share/applications/liftgate-desktop.desktop"
cp "${ICON_256}" "${DEB_ROOT}/usr/share/icons/hicolor/256x256/apps/liftgate-desktop.png"
cp "${ICON_512}" "${DEB_ROOT}/usr/share/icons/hicolor/512x512/apps/liftgate-desktop.png"
cp "${REPO_ROOT}/README.md" "${REPO_ROOT}/README.zh-CN.md" "${REPO_ROOT}/CHANGELOG.md" \
   "${REPO_ROOT}/LICENSE-MIT" "${REPO_ROOT}/LICENSE-APACHE" \
   "${DEB_ROOT}/usr/share/doc/liftgate-desktop/"

INSTALLED_KB="$(du -sk "${DEB_ROOT}/usr" | awk '{print $1}')"
cat > "${DEB_ROOT}/DEBIAN/control" <<EOF
Package: liftgate-desktop
Version: ${DEB_VER}
Section: devel
Priority: optional
Architecture: amd64
Installed-Size: ${INSTALLED_KB}
Depends: libc6 (>= 2.35), libfontconfig1, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libvulkan1
Maintainer: acking-you <noreply@github.com>
Homepage: https://github.com/acking-you/liftgate
Description: Native GPUI desktop control center and local API gateway for Antigravity
 Liftgate puts Google Antigravity behind standard OpenAI Responses and
 Anthropic Messages APIs via the official agy acp interface, bundled with
 the standalone liftgate-desktop GPUI control center and liftgate/lg CLIs.
EOF

DEB_OUT="${OUT_ABS}/liftgate-desktop-linux-amd64.deb"
dpkg-deb --build --root-owner-group "${DEB_ROOT}" "${DEB_OUT}"
dpkg-deb --info "${DEB_OUT}"

ls -lh "${OUT_ABS}"
