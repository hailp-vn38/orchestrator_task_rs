#!/usr/bin/env bash
# Dọn dẹp file rác khi build/test Rust để tiết kiệm dung lượng ổ cứng và giữ workspace sạch.
#
# Dùng:
#   scripts/clean.sh [prune|test-bins|all] [--quiet|-q] [--siblings]
#
# Chế độ:
#   prune (mặc định):
#     - Xóa incremental cache, target/flycheck*, target/tmp, *.rcgu.o, *.dSYM
#     - Xóa bản build cũ trùng lặp trong target/debug/deps (giữ bản mới nhất để không phải build lại)
#     - Xóa file tạm khi test (.DS_Store, *.sqlite-wal, *.sqlite-shm, *.swp, *.swo, scratch_tmp)
#   test-bins:
#     - Chạy prune + xóa toàn bộ binary test/example đã link (giữ lại .rlib/.rmeta của dependencies)
#   all:
#     - Chạy cargo clean hoàn toàn + dọn file tạm
#
# Cờ:
#   --quiet, -q : Chỉ in 1 dòng tóm tắt ngắn gọn (hoặc im lặng nếu không có gì thay đổi)
#   --siblings  : Dọn luôn các repo anh em (../orchestrator_agent_rs, ../llm-wiki-rs) nếu tồn tại

set -uo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MODE="prune"
QUIET=0
SIBLINGS=0

for arg in "$@"; do
  case "$arg" in
    prune|test-bins|all) MODE="$arg" ;;
    --quiet|-q) QUIET=1 ;;
    --siblings) SIBLINGS=1 ;;
    -h|--help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "clean: tham số không hợp lệ: $arg (dùng: prune | test-bins | all [--quiet] [--siblings])" >&2
      exit 1
      ;;
  esac
done

dir_size_kb() {
  local d="$1"
  if [ -d "$d" ]; then
    du -sk "$d" 2>/dev/null | awk '{print $1}'
  else
    echo 0
  fi
}

human_kb() {
  awk -v kb="${1:-0}" 'BEGIN {
    if (kb >= 1048576) printf "%.2fG", kb / 1048576;
    else if (kb >= 1024) printf "%.1fM", kb / 1024;
    else printf "%dK", kb;
  }'
}

# Dọn các bản build cũ trùng hash trong target/debug/deps
# - Với binary test (không có đuôi mở rộng): giữ 1 bản mới nhất cho mỗi tên test
# - Với .rlib / .rmeta / .d / .dylib: giữ tối đa 2 bản mới nhất cho mỗi crate (đề phòng host vs target / test-support feature)
prune_stale_deps() {
  local deps_dir="$1"
  local remove_all_test_bins="${2:-0}"
  [ -d "$deps_dir" ] || return 0

  python3 - "$deps_dir" "$remove_all_test_bins" <<'PY'
import os, re, sys

deps_dir = sys.argv[1]
remove_all_bins = sys.argv[2] == "1"
hash_re = re.compile(r"^(.+)-[0-9a-f]{16}(\..+)?$")

groups = {}
try:
    with os.scandir(deps_dir) as it:
        for entry in it:
            if not entry.is_file(follow_symlinks=False):
                continue
            m = hash_re.match(entry.name)
            if not m:
                continue
            prefix, ext = m.group(1), m.group(2) or ""
            try:
                mtime = entry.stat(follow_symlinks=False).st_mtime_ns
            except OSError:
                continue
            groups.setdefault((prefix, ext), []).append((mtime, entry.path))
except OSError:
    sys.exit(0)

for (prefix, ext), items in groups.items():
    if ext == "":
        # Binary thực thi (test binaries)
        if remove_all_bins:
            for _, p in items:
                try: os.unlink(p)
                except OSError: pass
        elif len(items) > 1:
            items.sort(reverse=True)
            for _, p in items[1:]:
                try: os.unlink(p)
                except OSError: pass
    else:
        # Library artifacts (.rlib, .rmeta, .d, .dylib, .so) -> giữ tối đa 2 hash mới nhất
        if len(items) > 2:
            items.sort(reverse=True)
            for _, p in items[2:]:
                try: os.unlink(p)
                except OSError: pass
PY
}

clean_repo() {
  local repo="$1"
  [ -d "$repo" ] || return 0
  local name
  name="$(basename "$repo")"
  local target_dir="$repo/target"
  local before_kb
  before_kb="$(dir_size_kb "$target_dir")"

  # 1. Dọn file tạm trong workspace (ngoài .git và target)
  find "$repo" \
    \( -path "$repo/.git" -o -path "$repo/target" \) -prune -o \
    -type f \( -name '.DS_Store' -o -name '*.swp' -o -name '*.swo' -o -name '*~' -o -name '*.sqlite-wal' -o -name '*.sqlite-shm' \) \
    -delete 2>/dev/null || true

  if [ -d "$repo/scratch_tmp" ]; then
    rm -rf "$repo/scratch_tmp"
  fi

  # 2. Dọn thư mục target theo MODE
  if [ "$MODE" = "all" ]; then
    if [ -f "$repo/Cargo.toml" ]; then
      (cd "$repo" && cargo clean --quiet 2>/dev/null) || rm -rf "$target_dir"
    else
      rm -rf "$target_dir"
    fi
  elif [ -d "$target_dir" ]; then
    # Xóa incremental cache, rust-analyzer flycheck, tmp
    rm -rf \
      "$target_dir/debug/incremental" \
      "$target_dir/release/incremental" \
      "$target_dir/tmp" \
      "$target_dir"/flycheck* 2>/dev/null || true

    # Xóa toàn bộ object lẻ (.rcgu.o) và gói debug rời (.dSYM)
    find "$target_dir" -type f -name '*.rcgu.o' -delete 2>/dev/null || true
    find "$target_dir" -type d -name '*.dSYM' -prune -exec rm -rf {} + 2>/dev/null || true

    if [ "$MODE" = "test-bins" ]; then
      prune_stale_deps "$target_dir/debug/deps" 1
      rm -rf "$target_dir/debug/examples" 2>/dev/null || true
    else
      prune_stale_deps "$target_dir/debug/deps" 0
    fi
  fi

  local after_kb
  after_kb="$(dir_size_kb "$target_dir")"
  local saved_kb=$(( before_kb > after_kb ? before_kb - after_kb : 0 ))

  if [ "$QUIET" -eq 0 ] || [ "$saved_kb" -gt 1024 ]; then
    echo "ok   clean($MODE) [$name]: $(human_kb "$before_kb") -> $(human_kb "$after_kb") (freed $(human_kb "$saved_kb"))"
  fi
}

clean_repo "$ROOT_DIR"

if [ "$SIBLINGS" -eq 1 ]; then
  for sib in "$ROOT_DIR/../orchestrator_agent_rs" "$ROOT_DIR/../llm-wiki-rs"; do
    if [ -d "$sib" ]; then
      clean_repo "$(cd "$sib" && pwd)"
    fi
  done
fi

