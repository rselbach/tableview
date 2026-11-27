#!/usr/bin/env bash
# Build, sign, notarize, staple, verify, and package TableView as a macOS DMG and update ZIP.
# Usage: scripts/release-macos.sh [--skip-notarize] [--version X.Y.Z] [--build N]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
readonly SOURCE_PLIST="${ROOT}/packaging/macos/Info.plist"
readonly UPDATE_REQUIREMENT="${ROOT}/crates/tv-update/update-requirement.txt"
readonly BUILD_DIR="${ROOT}/build"
readonly APP_DIR="${BUILD_DIR}/TableView.app"
readonly VOLUME_NAME="TableView"

STAGING=""
MOUNT_POINT=""

usage() {
  echo "usage: $0 [--skip-notarize] [--version X.Y.Z] [--build N]" >&2
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

# Preserve failing exit status in bash 3.2 (macOS /bin/bash).
cleanup() {
  local status=$?
  if [[ -n "${MOUNT_POINT}" ]]; then
    hdiutil detach -quiet "${MOUNT_POINT}" \
      || hdiutil detach -quiet -force "${MOUNT_POINT}" \
      || true
  fi
  if [[ -n "${STAGING}" ]]; then
    rm -rf "${STAGING}" || true
  fi
  exit "${status}"
}

require_tools() {
  local tools=(
    cargo
    codesign
    ditto
    hdiutil
    lipo
    openssl
    plutil
    shasum
    spctl
    xcrun
  )
  local tool
  for tool in "${tools[@]}"; do
    if ! command -v "${tool}" >/dev/null; then
      die "required command '${tool}' not found"
    fi
  done
  [[ -f "${UPDATE_REQUIREMENT}" ]] || die "missing update requirement file ${UPDATE_REQUIREMENT}"
}

uses_apple_id() {
  [[ -n "${APPLE_ID:-}" \
    && -n "${APPLE_TEAM_ID:-}" \
    && -n "${APPLE_APP_PASSWORD:-}" ]]
}

check_notary_credentials() {
  local partial="${APPLE_ID:-}${APPLE_TEAM_ID:-}${APPLE_APP_PASSWORD:-}"
  if [[ -n "${partial}" ]] && ! uses_apple_id; then
    die "set all of APPLE_ID, APPLE_TEAM_ID, and APPLE_APP_PASSWORD"
  fi
}

app_authority() {
  local info
  local authority
  info="$(codesign --display --verbose=2 "${APP_DIR}" 2>&1)"
  authority="$(sed -n 's/^Authority=//p' <<<"${info}" | head -n 1)"
  if [[ -z "${authority}" ]]; then
    echo "-"
    return
  fi
  echo "${authority}"
}

dmg_identity() {
  if [[ -n "${SIGNING_IDENTITY:-}" ]]; then
    echo "${SIGNING_IDENTITY}"
    return
  fi
  local cert_dir="${STAGING}/certs"
  mkdir -p "${cert_dir}"
  rm -f "${cert_dir}"/cert*
  codesign --display --extract-certificates="${cert_dir}/cert" \
    "${APP_DIR}" 2>/dev/null || true
  if [[ ! -s "${cert_dir}/cert0" ]]; then
    echo "-"
    return
  fi
  local sha1
  sha1="$(openssl x509 -inform DER -in "${cert_dir}/cert0" \
    -noout -fingerprint -sha1 | sed 's/^.*=//; s/://g')"
  rm -rf "${cert_dir}"
  if [[ ! "${sha1}" =~ ^[0-9A-Fa-f]{40}$ ]]; then
    die "failed to extract signing certificate SHA-1 fingerprint"
  fi
  echo "${sha1}"
}

retry_command() {
  local label="$1"
  local max_attempts="$2"
  shift 2
  local attempt
  local delay=5
  for ((attempt = 1; attempt <= max_attempts; attempt++)); do
    if "$@"; then
      return 0
    fi
    if (( attempt == max_attempts )); then
      return 1
    fi
    echo "${label} attempt ${attempt} failed; retrying in ${delay}s" >&2
    sleep "${delay}"
    delay=$((delay * 2))
  done
}

notarize() {
  local submission="$1"
  local target="$2"
  local label="$3"
  local auth=(--keychain-profile "${NOTARY_PROFILE:-tableview-notary}")
  if uses_apple_id; then
    auth=(
      --apple-id "${APPLE_ID}"
      --team-id "${APPLE_TEAM_ID}"
      --password "${APPLE_APP_PASSWORD}"
    )
  fi

  local log_dir="${NOTARIZATION_LOG_DIR:-${STAGING}}"
  mkdir -p "${log_dir}"
  local result="${log_dir}/${label}-submission.json"
  local log_file="${log_dir}/${label}.json"

  local submit_status=0
  if ! xcrun notarytool submit "${submission}" "${auth[@]}" --wait \
    --output-format json >"${result}"; then
    submit_status=$?
    echo "notarytool submit failed (${submit_status})" >&2
  fi

  local submission_id=""
  local status=""
  if [[ -s "${result}" ]]; then
    submission_id="$(plutil -extract id raw -o - "${result}" 2>/dev/null || true)"
    status="$(plutil -extract status raw -o - "${result}" 2>/dev/null || true)"
  fi

  if [[ -n "${submission_id}" ]]; then
    xcrun notarytool log "${submission_id}" "${auth[@]}" "${log_file}" \
      >/dev/null 2>&1 || true
  fi

  if [[ "${status}" != "Accepted" || "${submit_status}" -ne 0 ]]; then
    if [[ -s "${log_file}" ]]; then
      cat "${log_file}" >&2
    elif [[ -s "${result}" ]]; then
      cat "${result}" >&2
    fi
    die "notarization failed for ${label} (status: ${status:-unknown})"
  fi

  retry_command "stapler staple" 5 xcrun stapler staple "${target}"
  xcrun stapler validate "${target}"
}

notarize_app() {
  local submission="${STAGING}/TableView-notarize.zip"
  ditto -c -k --keepParent "${APP_DIR}" "${submission}"
  notarize "${submission}" "${APP_DIR}" "app"
  spctl --assess --type execute --verbose=4 "${APP_DIR}"
  rm -f "${submission}"
}

create_app_zip() {
  local zip="$1"
  rm -f "${zip}"
  ditto -c -k --keepParent "${APP_DIR}" "${zip}"
}

verify_app_zip() {
  local zip="$1"
  local version="$2"
  local build="$3"
  local notarized="$4"
  local unpack_dir="${STAGING}/unpack"

  rm -rf "${unpack_dir}"
  mkdir -p "${unpack_dir}"
  ditto -x -k "${zip}" "${unpack_dir}"

  local unpacked_app="${unpack_dir}/TableView.app"
  [[ -d "${unpacked_app}" ]] || die "ZIP has no TableView.app"

  local plist="${unpacked_app}/Contents/Info.plist"
  local got_version
  local got_build
  got_version="$(plutil -extract CFBundleShortVersionString raw -o - "${plist}")"
  got_build="$(plutil -extract CFBundleVersion raw -o - "${plist}")"
  if [[ "${got_version}" != "${version}" || "${got_build}" != "${build}" ]]; then
    die "ZIP holds ${got_version} (${got_build}), expected ${version} (${build})"
  fi

  codesign --verify --deep --strict --verbose=2 "${unpacked_app}"
  if [[ "${notarized}" == "true" ]]; then
    local req
    req="$(<"${UPDATE_REQUIREMENT}")"
    spctl --assess --type execute --verbose=2 "${unpacked_app}"
    codesign --verify --deep --strict -R="${req}" "${unpacked_app}"
  fi

  rm -rf "${unpack_dir}"
}

create_dmg() {
  local dmg="$1"
  local identity="$2"
  local root="${STAGING}/dmg"

  rm -rf "${root}"
  mkdir -p "${root}"
  ditto "${APP_DIR}" "${root}/TableView.app"
  ln -s /Applications "${root}/Applications"
  rm -f "${dmg}"

  hdiutil create -quiet \
    -volname "${VOLUME_NAME}" \
    -srcfolder "${root}" \
    -format UDZO \
    -ov \
    "${dmg}"

  if [[ "${identity}" == "-" ]]; then
    echo "app is signed ad hoc; leaving DMG unsigned" >&2
    return
  fi

  retry_command "codesign dmg" 5 \
    codesign --force --timestamp --sign "${identity}" "${dmg}"
  codesign --verify --strict --verbose=2 "${dmg}"
}

verify_dmg() {
  local dmg="$1"
  local version="$2"
  local build="$3"
  local notarized="$4"

  hdiutil verify -quiet "${dmg}"
  if [[ "${notarized}" == "true" ]]; then
    xcrun stapler validate "${dmg}"
    spctl --assess --type open --context context:primary-signature \
      --verbose=2 "${dmg}"
  fi

  MOUNT_POINT="${STAGING}/mount"
  mkdir -p "${MOUNT_POINT}"
  hdiutil attach -quiet -readonly -nobrowse -noautoopen \
    -mountpoint "${MOUNT_POINT}" "${dmg}"

  local mounted_app="${MOUNT_POINT}/TableView.app"
  [[ -d "${mounted_app}" ]] || die "DMG has no TableView.app"

  local link
  link="$(readlink "${MOUNT_POINT}/Applications" || true)"
  [[ "${link}" == "/Applications" ]] || die "DMG has no /Applications symlink"

  local plist="${mounted_app}/Contents/Info.plist"
  local got_version
  local got_build
  got_version="$(plutil -extract CFBundleShortVersionString raw -o - "${plist}")"
  got_build="$(plutil -extract CFBundleVersion raw -o - "${plist}")"
  if [[ "${got_version}" != "${version}" || "${got_build}" != "${build}" ]]; then
    die "DMG holds ${got_version} (${got_build}), expected ${version} (${build})"
  fi

  codesign --verify --deep --strict --verbose=2 "${mounted_app}"
  if [[ "${notarized}" == "true" ]]; then
    local req
    req="$(<"${UPDATE_REQUIREMENT}")"
    spctl --assess --type execute --verbose=2 "${mounted_app}"
    codesign --verify --deep --strict -R="${req}" "${mounted_app}"
  fi

  hdiutil detach -quiet "${MOUNT_POINT}"
  MOUNT_POINT=""
}

main() {
  local notarize_dmg="true"
  local version="${VERSION:-}"
  local build="${BUILD:-}"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --skip-notarize) notarize_dmg="false" ;;
      --version)
        [[ $# -ge 2 ]] || usage
        version="$2"
        shift
        ;;
      --build)
        [[ $# -ge 2 ]] || usage
        build="$2"
        shift
        ;;
      *) usage ;;
    esac
    shift
  done

  if [[ -z "${version}" ]]; then
    version="$(plutil -extract CFBundleShortVersionString raw -o - "${SOURCE_PLIST}")"
  fi
  if [[ -z "${build}" ]]; then
    build="$(plutil -extract CFBundleVersion raw -o - "${SOURCE_PLIST}")"
  fi
  if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    die "version '${version}' must match X.Y.Z or X.Y.Z-prerelease"
  fi
  if [[ ! "${build}" =~ ^[0-9]+$ ]]; then
    die "build '${build}' must be numeric"
  fi

  require_tools
  if [[ "${notarize_dmg}" == "true" ]]; then
    check_notary_credentials
  fi

  STAGING="$(mktemp -d)"
  trap cleanup EXIT

  VERSION="${version}" BUILD="${build}" \
    "${ROOT}/scripts/bundle-macos-app.sh" release

  local authority
  authority="$(app_authority)"
  if [[ "${notarize_dmg}" == "true" \
    && "${authority}" != "Developer ID Application"* ]]; then
    die "notarization requires Developer ID Application signature; got '${authority}'"
  fi

  local identity
  identity="$(dmg_identity)"

  if [[ "${notarize_dmg}" == "true" ]]; then
    notarize_app
  else
    echo "skipping notarization" >&2
  fi

  local zip_name="TableView-${version}-macos-arm64.zip"
  local zip="${BUILD_DIR}/${zip_name}"
  create_app_zip "${zip}"
  verify_app_zip "${zip}" "${version}" "${build}" "${notarize_dmg}"

  local dmg_name="TableView-${version}-macos-arm64.dmg"
  local dmg="${BUILD_DIR}/${dmg_name}"
  create_dmg "${dmg}" "${identity}"
  if [[ "${notarize_dmg}" == "true" ]]; then
    notarize "${dmg}" "${dmg}" "dmg"
  fi
  verify_dmg "${dmg}" "${version}" "${build}" "${notarize_dmg}"

  (
    cd "${BUILD_DIR}"
    shasum -a 256 "${zip_name}" >"${zip_name}.sha256"
    shasum -a 256 "${dmg_name}" >"${dmg_name}.sha256"
  )

  echo "built ${dmg} and ${zip} (${version} build ${build})"
}

main "$@"
