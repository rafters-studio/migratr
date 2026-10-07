#!/usr/bin/env bash
# install.sh: Installer for migratr (https://github.com/rafters-studio/migratr)
#
# Installs the newest published release unless a version is given. No version
# is written into this file, so every release updates the installer.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rafters-studio/migratr/main/install.sh | bash
#   curl -fsSL ... | bash -s v0.1.1
#   MIGRATR_VERSION=v0.1.1 bash install.sh
#   MIGRATR_INSTALL_DIR=/opt/bin bash install.sh

set -euo pipefail

REPO="rafters-studio/migratr"
BINARY_NAME="migratr"
INSTALL_DIR="${MIGRATR_INSTALL_DIR:-${HOME}/.local/bin}"
CARGO_HINT="install with: cargo install migratr-cli"

# Color support: only when stdout is a terminal.
if [ -t 1 ]; then
  RED='\033[0;31m'
  GREEN='\033[0;32m'
  YELLOW='\033[0;33m'
  BLUE='\033[0;34m'
  BOLD='\033[1m'
  RESET='\033[0m'
else
  RED=''
  GREEN=''
  YELLOW=''
  BLUE=''
  BOLD=''
  RESET=''
fi

info() {
  printf "${BLUE}info${RESET}: %s\n" "$1" >&2
}

warn() {
  printf "${YELLOW}warn${RESET}: %s\n" "$1" >&2
}

error() {
  printf "${RED}error${RESET}: %s\n" "$1" >&2
  exit 1
}

success() {
  printf "${GREEN}success${RESET}: %s\n" "$1" >&2
}

# Temp dir with cleanup trap.
TMPDIR_INSTALL=""
cleanup() {
  if [ -n "${TMPDIR_INSTALL}" ] && [ -d "${TMPDIR_INSTALL}" ]; then
    rm -rf "${TMPDIR_INSTALL}"
  fi
}
trap cleanup EXIT INT TERM

# Detect HTTP client: prefer curl, fall back to wget.
FETCH=""
detect_http_client() {
  if [ -n "${FETCH}" ]; then return; fi
  if command -v curl >/dev/null 2>&1; then
    FETCH="curl"
  elif command -v wget >/dev/null 2>&1; then
    FETCH="wget"
  else
    error "Neither curl nor wget found. Please install one and retry."
  fi
}

fetch() {
  local url="$1"
  local output="$2"
  detect_http_client
  if [ "${FETCH}" = "curl" ]; then
    curl -fsSL -o "${output}" "${url}" || error "Could not fetch ${url}"
  else
    wget -q -O "${output}" "${url}" || error "Could not fetch ${url}"
  fi
}

# Follow a redirect to get the final URL (used for version resolution).
fetch_redirect_url() {
  local url="$1"
  detect_http_client
  if [ "${FETCH}" = "curl" ]; then
    curl -fsSL -o /dev/null -w '%{url_effective}' "${url}" || true
  else
    wget --spider -S -O /dev/null "${url}" 2>&1 | grep -i 'Location:' | tail -1 | awk '{print $2}' | tr -d '\r'
  fi
}

# Detect platform.
detect_platform() {
  local os
  os="$(uname -s)"
  case "${os}" in
    Linux)  echo "linux" ;;
    Darwin) echo "macos" ;;
    *)      error "Unsupported operating system: ${os}. migratr builds exist for Linux and macOS only; ${CARGO_HINT}" ;;
  esac
}

# Detect architecture, with Rosetta 2 detection on macOS.
detect_arch() {
  local arch platform
  arch="$(uname -m)"
  platform="$1"

  # On macOS, check for Rosetta 2 translation. If the process is translated,
  # the real hardware is arm64, so prefer the native build.
  if [ "${platform}" = "macos" ]; then
    local translated
    translated="$(sysctl -n sysctl.proc_translated 2>/dev/null || echo "0")"
    if [ "${translated}" = "1" ]; then
      warn "Rosetta 2 translation detected. Installing native arm64 binary."
      echo "arm64"
      return
    fi
  fi

  case "${arch}" in
    x86_64|amd64)   echo "x64" ;;
    arm64|aarch64)   echo "arm64" ;;
    *)               error "Unsupported architecture: ${arch}; ${CARGO_HINT}" ;;
  esac
}

# Resolve the version tag to install.
resolve_version() {
  # Priority: CLI argument, then env var, then latest release.
  local version="${1:-${MIGRATR_VERSION:-}}"
  if [ -n "${version}" ]; then
    # Ensure the version starts with 'v'.
    case "${version}" in
      v*) echo "${version}" ;;
      *)  echo "v${version}" ;;
    esac
    return
  fi

  info "Resolving latest version from GitHub..."
  local redirect_url
  redirect_url="$(fetch_redirect_url "https://github.com/${REPO}/releases/latest")"
  if [ -z "${redirect_url}" ]; then
    error "Could not resolve the latest migratr release. Specify a version: bash -s v0.1.1"
  fi

  # The redirect URL ends with the tag, e.g. .../releases/tag/v0.1.0
  local tag
  tag="${redirect_url##*/}"
  case "${tag}" in
    v[0-9]*) ;;
    *) error "Could not resolve the latest migratr release. Specify a version: bash -s v0.1.1" ;;
  esac
  echo "${tag}"
}

# Verify the SHA-256 of the downloaded archive against checksums.txt.
# Exits non-zero, naming the archive, on a missing line or a mismatch.
verify_checksum() {
  local archive="$1"
  local checksums_file="$2"
  local platform="$3"
  local archive_name archive_dir
  archive_name="$(basename "${archive}")"
  archive_dir="$(dirname "${archive}")"

  info "Verifying checksum..."

  # checksums.txt lines are "<hash>  <name>"; match the name field exactly.
  local expected_line
  expected_line="$(awk -v n="${archive_name}" '$2 == n { print; exit }' "${checksums_file}")"
  if [ -z "${expected_line}" ]; then
    error "No checksum found for ${archive_name} in checksums.txt."
  fi

  local check_file
  check_file="$(mktemp)"
  echo "${expected_line}" > "${check_file}"

  # Use the platform-appropriate checksum tool.
  local checksum_ok=true
  if [ "${platform}" = "macos" ]; then
    (cd "${archive_dir}" && shasum -a 256 -c "${check_file}" >/dev/null 2>&1) || checksum_ok=false
  else
    (cd "${archive_dir}" && sha256sum -c "${check_file}" >/dev/null 2>&1) || checksum_ok=false
  fi
  rm -f "${check_file}"

  if [ "${checksum_ok}" = "false" ]; then
    error "Checksum verification failed for ${archive_name}. The download may be corrupted."
  fi
  success "Checksum verified."
}

# Add the install dir to PATH in the appropriate shell config (idempotent).
ensure_path() {
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) return ;;
  esac

  local shell_name
  shell_name="$(basename "${SHELL:-bash}")"
  local path_line=""
  local config_file=""

  case "${shell_name}" in
    bash) config_file="${HOME}/.bashrc" ;;
    zsh)  config_file="${HOME}/.zshrc" ;;
    fish)
      config_file="${HOME}/.config/fish/config.fish"
      path_line="fish_add_path ${INSTALL_DIR}"
      ;;
    *)
      warn "${INSTALL_DIR} is not in your PATH. Add it manually to your shell config."
      return
      ;;
  esac

  [ -z "${path_line}" ] && path_line="export PATH=\"${INSTALL_DIR}:\${PATH}\""

  # Only append if the line is not already present.
  if [ -f "${config_file}" ] && grep -qF "${path_line}" "${config_file}" 2>/dev/null; then
    return
  fi

  info "Adding ${INSTALL_DIR} to PATH in ${config_file}..."
  if mkdir -p "$(dirname "${config_file}")" 2>/dev/null && \
     printf '\n# Added by migratr installer\n%s\n' "${path_line}" >> "${config_file}" 2>/dev/null; then
    warn "Restart your shell or run:  source ${config_file}"
  else
    warn "Could not update ${config_file}. Add ${INSTALL_DIR} to your PATH manually."
  fi
}

# Main installation flow.
main() {
  local version="${1:-}"

  printf '%b\n\n' "${BOLD}migratr Installer${RESET}" >&2

  local platform arch artifact_name
  platform="$(detect_platform)" || exit 1
  arch="$(detect_arch "${platform}")" || exit 1
  artifact_name="${BINARY_NAME}-${platform}-${arch}"

  # Reject unsupported combinations before any download.
  if [ "${platform}" = "linux" ] && [ "${arch}" = "arm64" ]; then
    error "linux-arm64 builds are not published; ${CARGO_HINT}"
  fi

  info "Detected platform: ${platform}-${arch}"

  version="$(resolve_version "${version}")" || exit 1
  info "Installing migratr ${version}..."

  TMPDIR_INSTALL="$(mktemp -d)"

  local base_url="https://github.com/${REPO}/releases/download/${version}"
  local archive_file="${artifact_name}.tar.gz"
  local archive_path="${TMPDIR_INSTALL}/${archive_file}"
  local checksums_path="${TMPDIR_INSTALL}/checksums.txt"

  info "Downloading ${archive_file}..."
  fetch "${base_url}/${archive_file}" "${archive_path}"
  fetch "${base_url}/checksums.txt" "${checksums_path}"

  # Verify integrity before anything is installed.
  verify_checksum "${archive_path}" "${checksums_path}" "${platform}"

  info "Installing to ${INSTALL_DIR}..."
  mkdir -p "${INSTALL_DIR}"
  tar xzf "${archive_path}" -C "${TMPDIR_INSTALL}"
  mv "${TMPDIR_INSTALL}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
  chmod +x "${INSTALL_DIR}/${BINARY_NAME}"

  ensure_path

  local installed_version
  installed_version="$("${INSTALL_DIR}/${BINARY_NAME}" --version 2>/dev/null || true)"
  if [ -z "${installed_version}" ]; then
    installed_version="migratr ${version#v}"
  fi
  success "Installed ${installed_version} to ${INSTALL_DIR}/${BINARY_NAME}"
}

# main runs unless the test harness sources the file. No BASH_SOURCE guard:
# under `curl | bash` BASH_SOURCE[0] is empty and main would never run.
if [ -z "${MIGRATR_INSTALL_SOURCED:-}" ]; then
  main "$@"
fi
