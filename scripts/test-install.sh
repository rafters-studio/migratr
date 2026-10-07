#!/usr/bin/env bash
# Runs install.sh against real published releases into temp install dirs.
# Needs network. Usage: bash scripts/test-install.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="${ROOT}/install.sh"
PIN="v0.1.1"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

fails=0
pass() { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; fails=$((fails + 1)); }
expect_eq() { # name got want
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (got '$2', want '$3')"; fi
}
new_dir() { mktemp -d "${WORK}/dir.XXXXXX"; }
# Keep installs from touching the real shell config.
export HOME="${WORK}/home"
mkdir -p "${HOME}"

# 1. Pinned install by argument.
d="$(new_dir)"
MIGRATR_INSTALL_DIR="${d}" bash "${INSTALLER}" "${PIN}" >/dev/null 2>&1 || true
expect_eq "pinned install by argument" "$("${d}/migratr" --version 2>/dev/null || true)" "migratr ${PIN#v}"

# 2. Piped install.
d="$(new_dir)"
cat "${INSTALLER}" | MIGRATR_INSTALL_DIR="${d}" bash -s "${PIN}" >/dev/null 2>&1 || true
expect_eq "piped install" "$("${d}/migratr" --version 2>/dev/null || true)" "migratr ${PIN#v}"

# 3. Latest: the tag releases/latest redirects to.
latest_url="$(curl -fsSL -o /dev/null -w '%{url_effective}' https://github.com/rafters-studio/migratr/releases/latest)"
latest_tag="${latest_url##*/}"
d="$(new_dir)"
MIGRATR_INSTALL_DIR="${d}" bash "${INSTALLER}" >/dev/null 2>&1 || true
expect_eq "latest install" "$("${d}/migratr" --version 2>/dev/null || true)" "migratr ${latest_tag#v}"

# 4. MIGRATR_VERSION installs; a positional argument beats it.
d="$(new_dir)"
MIGRATR_VERSION="${PIN}" MIGRATR_INSTALL_DIR="${d}" bash "${INSTALLER}" >/dev/null 2>&1 || true
expect_eq "MIGRATR_VERSION installs" "$("${d}/migratr" --version 2>/dev/null || true)" "migratr ${PIN#v}"
d="$(new_dir)"
MIGRATR_VERSION="v9.9.9" MIGRATR_INSTALL_DIR="${d}" bash "${INSTALLER}" "${PIN}" >/dev/null 2>&1 || true
expect_eq "argument beats MIGRATR_VERSION" "$("${d}/migratr" --version 2>/dev/null || true)" "migratr ${PIN#v}"

# 5. Missing tag: non-zero, names the URL, installs nothing.
d="$(new_dir)"
if out="$(MIGRATR_INSTALL_DIR="${d}" bash "${INSTALLER}" v9.9.9 2>&1)"; then
  fail "missing tag exits non-zero"
else
  pass "missing tag exits non-zero"
fi
case "${out}" in
  *"releases/download/v9.9.9/"*) pass "missing tag names the URL" ;;
  *) fail "missing tag names the URL (output: ${out})" ;;
esac
expect_eq "missing tag installs nothing" "$(ls -A "${d}")" ""

# Function-level cases: source the installer without running main.
export MIGRATR_INSTALL_SOURCED=1
# shellcheck source=../install.sh
source "${INSTALLER}"
unset MIGRATR_INSTALL_SOURCED
set +e

if [ "$(uname -s)" = "Darwin" ]; then plat=macos; hash_of() { shasum -a 256 "$1" | awk '{print $1}'; }
else plat=linux; hash_of() { sha256sum "$1" | awk '{print $1}'; }; fi

# 6. Checksum cases.
d="$(new_dir)"
printf 'payload' > "${WORK}/migratr-test.tar.gz"
good="$(hash_of "${WORK}/migratr-test.tar.gz")"
printf '%s  migratr-test.tar.gz\n' "${good}" > "${WORK}/good.txt"
printf '%s  migratr-test.tar.gz\n' "$(printf '%064d' 0)" > "${WORK}/bad.txt"
printf '%s  other.tar.gz\n' "${good}" > "${WORK}/missing.txt"

(verify_checksum "${WORK}/migratr-test.tar.gz" "${WORK}/good.txt" "${plat}") >/dev/null 2>&1
expect_eq "verify_checksum accepts a correct hash" "$?" "0"

out="$( (verify_checksum "${WORK}/migratr-test.tar.gz" "${WORK}/bad.txt" "${plat}") 2>&1 )"; rc=$?
if [ "${rc}" -ne 0 ] && [[ "${out}" == *migratr-test.tar.gz* ]]; then pass "mismatch refuses, naming the archive"
else fail "mismatch refuses, naming the archive (rc=${rc})"; fi
expect_eq "mismatch leaves the install dir empty" "$(ls -A "${d}")" ""

out="$( (verify_checksum "${WORK}/migratr-test.tar.gz" "${WORK}/missing.txt" "${plat}") 2>&1 )"; rc=$?
if [ "${rc}" -ne 0 ] && [[ "${out}" == *migratr-test.tar.gz* ]]; then pass "missing line refuses, naming the archive"
else fail "missing line refuses, naming the archive (rc=${rc})"; fi

# 7. Platform mapping and refusals (uname and sysctl stubbed, no download).
stub() { # os arch translated
  uname() { if [ "$1" = "-s" ]; then echo "$STUB_OS"; else echo "$STUB_ARCH"; fi; }
  sysctl() { echo "$STUB_TRANSLATED"; }
}
stub
map() { STUB_OS="$1" STUB_ARCH="$2" STUB_TRANSLATED="${3:-0}"; p="$(detect_platform)"; a="$(detect_arch "$p" 2>/dev/null)"; echo "${BINARY_NAME}-${p}-${a}"; }
expect_eq "Linux x86_64" "$(map Linux x86_64)" "migratr-linux-x64"
expect_eq "macOS x86_64" "$(map Darwin x86_64)" "migratr-macos-x64"
expect_eq "macOS arm64" "$(map Darwin arm64)" "migratr-macos-arm64"
expect_eq "macOS under Rosetta" "$(map Darwin x86_64 1)" "migratr-macos-arm64"

for case_ in "Linux aarch64" "FreeBSD x86_64"; do
  set -- ${case_}
  out="$( (STUB_OS="$1" STUB_ARCH="$2" STUB_TRANSLATED=0; MIGRATR_INSTALL_DIR="$(new_dir)"; export MIGRATR_INSTALL_DIR
           main) 2>&1 )"; rc=$?
  if [ "${rc}" -ne 0 ] && [[ "${out}" == *"cargo install migratr-cli"* ]] && [[ "${out}" != *Downloading* ]]; then
    pass "${case_} refused before download"
  else fail "${case_} refused before download (rc=${rc}: ${out})"; fi
done

if [ "${fails}" -ne 0 ]; then printf '%d failure(s)\n' "${fails}"; exit 1; fi
printf 'all install tests passed\n'
