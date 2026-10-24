"""textstat 命令行入口：读文本，打印（或写出）统计报告。"""

import argparse
import sys
from pathlib import Path

from textstat.core import char_count, line_count, top_words, word_count


def build_report(text: str, source: str, top_n: int = 5) -> str:
    lines = [
        "文本统计报告",
        f"来源: {source}",
        f"行数: {line_count(text)}",
        f"词数: {word_count(text)}",
        f"字符(含空白): {char_count(text)}",
        f"字符(不含空白): {char_count(text, include_spaces=False)}",
        f"高频词 (前 {top_n}，已过滤停用词):",
    ]
    ranked = top_words(text, n=top_n)
    if not ranked:
        lines.append("  (无)")
    for i, (word, count) in enumerate(ranked, 1):
        lines.append(f"  {i}. {word} {count}")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="统计一个文本文件的行数、词数与高频词")
    parser.add_argument("path", nargs="?", help="文本文件路径；省略则读 stdin")
    parser.add_argument("--top", type=int, default=5, help="高频词数量，默认 5")
    parser.add_argument("--out", help="把报告写到这个文件")
    args = parser.parse_args(argv)

    if args.path:
        path = Path(args.path)
        if not path.is_file():
            print(f"找不到文件: {path}", file=sys.stderr)
            return 2
        text = path.read_text(encoding="utf-8")
        source = str(path)
    else:
        text = sys.stdin.read()
        source = "<stdin>"

    report = build_report(text, source, top_n=args.top)
    if args.out:
        Path(args.out).write_text(report, encoding="utf-8")
        print(f"报告已写入 {args.out}")
    else:
        sys.stdout.write(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
