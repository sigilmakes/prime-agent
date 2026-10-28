#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

find_browser() {
  if [[ -n "${BROWSER:-}" ]]; then
    echo "${BROWSER}"
    return
  fi

  if command -v google-chrome >/dev/null 2>&1; then
    command -v google-chrome
    return
  fi

  if command -v google-chrome-stable >/dev/null 2>&1; then
    command -v google-chrome-stable
    return
  fi

  if command -v chromium >/dev/null 2>&1; then
    command -v chromium
    return
  fi

  if command -v chromium-browser >/dev/null 2>&1; then
    command -v chromium-browser
    return
  fi

  if [[ -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ]]; then
    echo "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    return
  fi

  echo "Error: Could not locate a Chrome/Chromium browser binary." >&2
  echo "Set BROWSER=/path/to/chrome and retry." >&2
  exit 1
}

platform_name() {
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) echo "mac-arm64" ;;
    Darwin-x86_64) echo "mac-x64" ;;
    Linux-x86_64) echo "linux64" ;;
    Linux-aarch64) echo "linux-arm64" ;;
    *)
      echo "Error: Unsupported platform for chromedriver auto-download: $(uname -s)-$(uname -m)" >&2
      exit 1
      ;;
  esac
}

resolve_chromedriver() {
  if [[ -n "${CHROMEDRIVER:-}" ]]; then
    echo "${CHROMEDRIVER}"
    return
  fi

  local browser
  browser="$(find_browser)"

  local browser_version
  browser_version="$("${browser}" --version | sed -E 's/.* ([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+).*/\1/')"
  if [[ -z "${browser_version}" ]]; then
    echo "Error: Could not determine browser version from: ${browser}" >&2
    exit 1
  fi

  local major_version
  major_version="${browser_version%%.*}"
  local platform
  platform="$(platform_name)"

  local release_version
  release_version="$(curl -fsSL "https://googlechromelabs.github.io/chrome-for-testing/LATEST_RELEASE_${major_version}")"
  if [[ -z "${release_version}" ]]; then
    echo "Error: Could not resolve chromedriver release for Chrome major ${major_version}" >&2
    exit 1
  fi

  local cache_root="${ROOT_DIR}/target/chromedriver"
  local extract_root="${cache_root}/${release_version}"
  local driver_dir="${extract_root}/chromedriver-${platform}"
  local driver_bin="${driver_dir}/chromedriver"
  local zip_path="${cache_root}/chromedriver-${release_version}-${platform}.zip"
  local download_url="https://storage.googleapis.com/chrome-for-testing-public/${release_version}/${platform}/chromedriver-${platform}.zip"

  if [[ ! -x "${driver_bin}" ]]; then
    mkdir -p "${cache_root}" "${extract_root}"
    curl -fsSL -o "${zip_path}" "${download_url}"
    unzip -qo "${zip_path}" -d "${extract_root}"
    chmod +x "${driver_bin}"
  fi

  echo "${driver_bin}"
}

find_wasm_bindgen_test_runner() {
  if [[ -n "${WASM_BINDGEN_TEST_RUNNER:-}" ]]; then
    echo "${WASM_BINDGEN_TEST_RUNNER}"
    return
  fi

  if command -v wasm-bindgen-test-runner >/dev/null 2>&1; then
    command -v wasm-bindgen-test-runner
    return
  fi

  local cache_candidates=(
    "${HOME}/Library/Caches/.wasm-pack"
    "${XDG_CACHE_HOME:-${HOME}/.cache}/.wasm-pack"
  )

  local cache_dir
  for cache_dir in "${cache_candidates[@]}"; do
    if [[ -d "${cache_dir}" ]]; then
      local runner
      runner="$(find "${cache_dir}" -maxdepth 3 -type f -name wasm-bindgen-test-runner | sort | tail -n 1)"
      if [[ -n "${runner}" ]]; then
        echo "${runner}"
        return
      fi
    fi
  done

  echo "Error: Could not find wasm-bindgen-test-runner. Run 'just wasm-build' first." >&2
  exit 1
}

browser_bin="$(find_browser)"
chromedriver_bin="$(resolve_chromedriver)"
runner_bin="$(find_wasm_bindgen_test_runner)"

cd "${ROOT_DIR}"
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER="${runner_bin}" \
CHROMEDRIVER="${chromedriver_bin}" \
BROWSER="${browser_bin}" \
WASM_BINDGEN_TEST_ONLY_WEB=1 \
cargo test -p mmdflux-wasm --target wasm32-unknown-unknown --test web
