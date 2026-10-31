#!/usr/bin/env python3
"""Liệt kê file code vượt ngưỡng dòng và tạo báo cáo Markdown (cần rg)."""

import argparse
from pathlib import Path
import subprocess


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--threshold", type=int, default=500, help="Ngưỡng số dòng (mặc định: 500)"
    )
    parser.add_argument(
        "--output", default="docs/long-code-files.md",
        help="File báo cáo, đường dẫn tương đối tính từ gốc project",
    )
    args = parser.parse_args()
    if args.threshold < 0:
        parser.error("--threshold phải không âm")

    try:
        result = subprocess.run(
            ["rg", "--files", "--hidden", "-0", "-g", "!.git/",
             "-g", "*.rs", "-g", "*.sh", "-g", "*.py"],
            cwd=root, capture_output=True, check=False,
        )
    except FileNotFoundError:
        parser.exit(1, "Cần cài ripgrep (rg) để chạy script.\n")
    if result.returncode not in (0, 1):
        parser.exit(1, result.stderr.decode(errors="replace"))

    matches = []
    for raw_path in result.stdout.split(b"\0"):
        if not raw_path:
            continue
        relative = Path(raw_path.decode())
        with (root / relative).open("rb") as source:
            lines = sum(1 for _ in source)
        if lines > args.threshold:
            matches.append((relative.as_posix(), lines))
    matches.sort(key=lambda item: (-item[1], item[0]))

    report = [
        f"# File code dài hơn {args.threshold} dòng", "",
        "Phạm vi: file `.rs`, `.sh`, `.py` trong project; tuân theo ignore của ripgrep/Git.",
        "Đếm toàn bộ dòng, gồm dòng trống, comment và dòng cuối chưa có newline.", "",
        "Tạo lại bằng: "
        f"`python3 scripts/list-long-code-files.py --threshold {args.threshold}`.", "",
        f"Tổng cộng: **{len(matches)} file** (sắp xếp số dòng giảm dần).", "",
        "| Path | Số dòng |", "| --- | ---: |",
    ]
    for path, lines in matches:
        report.append(f"| `{path.replace('|', '&#124;')}` | {lines} |")
        print(f"{lines:>6}  {path}")

    output = root / args.output
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("\n".join(report) + "\n", encoding="utf-8")
    print(f"Đã ghi {len(matches)} file vào {output}")


if __name__ == "__main__":
    main()
