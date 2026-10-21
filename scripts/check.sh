#!/usr/bin/env bash
# Build/test/run gate với output tối thiểu (tiết kiệm context cho agent):
# - Im lặng khi pass (chỉ in 1 dòng `ok   <step>`)
# - Chỉ in lỗi / warning / test fail đã lọc nhiễu khi có vấn đề
# - Tự động dọn file build/test thừa (prune) sau khi test xong
#
# Dùng:
#   scripts/check.sh [fast|full]
#   scripts/check.sh test [cargo-test-args...]
#   scripts/check.sh check [cargo-check-args...]
#   scripts/check.sh clippy [cargo-clippy-args...]
#   scripts/check.sh run [cargo-run-args...]
#   scripts/check.sh clean [prune|test-bins|all] [--siblings]
#
# Biến môi trường:
#   CHECK_MAX_LINES=60   Số dòng log tối đa hiển thị khi lỗi (mặc định: 60)
#   CHECK_AUTO_CLEAN=1   Tự động chạy clean.sh prune --quiet sau khi test (1=bật, 0=tắt)

set -uo pipefail
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

mode="${1:-fast}"
if [ $# -gt 0 ]; then
  shift
fi

if [ "$mode" = "clean" ]; then
  exec "$ROOT_DIR/scripts/clean.sh" "$@"
fi

if [ "$mode" = "-h" ] || [ "$mode" = "--help" ]; then
  sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
fi

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
max_lines="${CHECK_MAX_LINES:-60}"
auto_clean="${CHECK_AUTO_CLEAN:-1}"

run() { # run <name> <cmd...>
  local name="$1"; shift
  if "$@" >"$log" 2>&1; then
    echo "ok   $name"
  else
    local code=$?
    echo "FAIL $name (exit $code)"
    # Chỉ giữ dòng hữu ích: lỗi, warning, vị trí file:line, panic/test fail; bỏ dòng tiến trình Cargo và test pass
    grep -Ev '^\s*(Compiling|Checking|Fresh|Finished|Running|Downloaded|Downloading|Blocking|Locking|Updating|Adding|Doc-tests)\b|^\s*$|^running [0-9]+ tests?$|^test .* \.\.\. ok$|^test result: ok\.' "$log" \
      | head -n "$max_lines"
    echo "(log rút gọn tối đa ${max_lines} dòng; đặt CHECK_MAX_LINES để tăng)"
    exit "$code"
  fi
}

auto_prune() {
  if [ "$auto_clean" = "1" ] && [ -x "$ROOT_DIR/scripts/clean.sh" ]; then
    "$ROOT_DIR/scripts/clean.sh" prune --quiet
  fi
}

case "$mode" in
  fast)
    run fmt    cargo fmt --check
    run clippy cargo clippy --all-targets --quiet -- -D warnings
    run test   cargo test --lib --tests --quiet
    auto_prune
    echo "ALL OK (fast)"
    ;;
  full)
    run fmt    cargo fmt --check
    run clippy cargo clippy --all-targets --quiet -- -D warnings
    run test   cargo test --lib --tests --quiet
    if [ -d "$ROOT_DIR/examples" ]; then
      run examples cargo build --examples --quiet
    fi
    run doctest cargo test --doc --quiet
    auto_prune
    echo "ALL OK (full)"
    ;;
  test)
    if [ $# -eq 0 ]; then
      run test cargo test --lib --tests --quiet
    else
      run "test($*)" cargo test --quiet "$@"
    fi
    auto_prune
    ;;
  check)
    run check cargo check --all-targets --quiet "$@"
    ;;
  clippy)
    run clippy cargo clippy --all-targets --quiet "$@" -- -D warnings
    ;;
  run)
    run run cargo run --quiet "$@"
    ;;
  *)
    echo "check: chế độ không hợp lệ '$mode' (dùng: fast | full | test | check | clippy | run | clean)" >&2
    exit 1
    ;;
esac

