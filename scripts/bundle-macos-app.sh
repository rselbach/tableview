#!/usr/bin/env bash
# Build and assemble a signed build/TableView.app bundle on macOS.
# Usage: scripts/bundle-macos-app.sh [--binary PATH] [debug|release]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
readonly PACKAGING_DIR="${ROOT}/packaging/macos"
readonly SOURCE_PLIST="${PACKAGING_DIR}/Info.plist"
readonly SOURCE_ICON="${PACKAGING_DIR}/AppIcon.icns"
readonly BUILD_DIR="${ROOT}/build"
readonly APP_DIR="${BUILD_DIR}/TableView.app"
readonly DEVELOPER_ID="Developer ID Application"

usage() {
  echo "usage: $0 [--binary PATH] [debug|release]" >&2
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

require_tools() {
  local tools=(cargo codesign lipo plutil)
  local tool
  for tool in "${tools[@]}"; do
    if ! command -v "${tool}" >/dev/null; then
      die "required command '${tool}' not found"
    fi
  done
}

resolve_signing_identity() {
  if [[ -n "${SIGNING_IDENTITY:-}" ]]; then
    echo "${SIGNING_IDENTITY}"
    return
  fi
  local identities
  local sha1
  identities="$(security find-identity -v -p codesigning 2>/dev/null || true)"
  sha1="$(awk -v dev="${DEVELOPER_ID}" \
    '$0 ~ "\"" dev && $2 ~ /^[0-9A-Fa-f]{40}$/ { print $2; exit }' <<<"${identities}")"
  if [[ -n "${sha1}" ]]; then
    echo "${sha1}"
    return
  fi
  echo "-"
}

codesign_run() {
  local retries="$1"
  shift
  local attempt
  local delay=5
  for ((attempt = 1; attempt <= retries; attempt++)); do
    if codesign "$@"; then
      return 0
    fi
    if (( attempt == retries )); then
      return 1
    fi
    echo "codesign attempt ${attempt} failed; retrying in ${delay}s" >&2
    sleep "${delay}"
    delay=$((delay * 2))
  done
}

build_binary() {
  local profile="$1"
  local cargo_args=(build --locked -p tv-app)
  if [[ "${profile}" == "release" ]]; then
    cargo_args+=(--release)
  fi
  cargo "${cargo_args[@]}"
}

stamp_version() {
  local plist="$1"
  if [[ -n "${VERSION:-}" ]]; then
    plutil -replace CFBundleShortVersionString -string "${VERSION}" "${plist}"
  fi
  if [[ -n "${BUILD:-}" ]]; then
    plutil -replace CFBundleVersion -string "${BUILD}" "${plist}"
  fi
  plutil -lint -s "${plist}"
}

assemble_bundle() {
  local binary_path="$1"
  local contents="${APP_DIR}/Contents"
  local exe_name
  exe_name="$(plutil -extract CFBundleExecutable raw -o - "${SOURCE_PLIST}")"
  [[ -n "${exe_name}" ]] || die "missing CFBundleExecutable in ${SOURCE_PLIST}"

  rm -rf "${APP_DIR}"
  install -d "${contents}/MacOS" "${contents}/Resources"
  install -m 0755 "${binary_path}" "${contents}/MacOS/${exe_name}"
  install -m 0644 "${SOURCE_PLIST}" "${contents}/Info.plist"
  install -m 0644 "${SOURCE_ICON}" "${contents}/Resources/AppIcon.icns"
  printf 'APPL????' >"${contents}/PkgInfo"
  stamp_version "${contents}/Info.plist"
}

sign_bundle() {
  local identity="$1"
  local profile="$2"
  local exe_name
  exe_name="$(plutil -extract CFBundleExecutable raw -o - "${SOURCE_PLIST}")"

  local flags=(--force --options runtime --sign "${identity}")
  local retries=1
  if [[ "${identity}" != "-" && "${profile}" == "release" ]]; then
    flags+=(--timestamp)
    retries=5
  else
    flags+=(--timestamp=none)
  fi

  codesign_run "${retries}" "${flags[@]}" "${APP_DIR}/Contents/MacOS/${exe_name}"
  codesign_run "${retries}" "${flags[@]}" "${APP_DIR}"
  codesign --verify --deep --strict --verbose=2 "${APP_DIR}"
}

main() {
  local binary_override=""
  local profile="debug"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --binary)
        [[ $# -ge 2 ]] || usage
        binary_override="$2"
        shift
        ;;
      debug | release)
        profile="$1"
        ;;
      *)
        usage
        ;;
    esac
    shift
  done

  if [[ -n "${VERSION:-}" && ! "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    die "VERSION '${VERSION}' must match X.Y.Z or X.Y.Z-prerelease"
  fi
  if [[ -n "${BUILD:-}" && ! "${BUILD}" =~ ^[0-9]+$ ]]; then
    die "BUILD '${BUILD}' must be numeric"
  fi

  require_tools
  [[ -f "${SOURCE_PLIST}" ]] || die "missing ${SOURCE_PLIST}"
  [[ -f "${SOURCE_ICON}" ]] || die "missing ${SOURCE_ICON}"

  local deployment_target
  deployment_target="$(plutil -extract LSMinimumSystemVersion raw -o - "${SOURCE_PLIST}")"
  export MACOSX_DEPLOYMENT_TARGET="${deployment_target}"

  local binary_path
  if [[ -n "${binary_override}" ]]; then
    binary_path="${binary_override}"
  else
    build_binary "${profile}"
    local target_dir="${CARGO_TARGET_DIR:-${ROOT}/target}"
    binary_path="${target_dir}/${profile}/tv-app"
  fi

  [[ -f "${binary_path}" ]] || die "binary not found: ${binary_path}"
  local archs
  archs="$(lipo -archs "${binary_path}")"
  if [[ "${archs}" != *"arm64"* ]]; then
    die "binary architectures '${archs}' do not include arm64"
  fi

  assemble_bundle "${binary_path}"

  local identity
  identity="$(resolve_signing_identity)"
  if [[ "${identity}" == "-" ]]; then
    echo "no ${DEVELOPER_ID} identity; signing ad hoc" >&2
  fi
  sign_bundle "${identity}" "${profile}"
  echo "built ${APP_DIR} (${profile})"
}

main "$@"
