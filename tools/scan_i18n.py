# -*- coding: utf-8 -*-
"""扫描 Rust 源码里的「面向用户的中文字符串字面量」（排除注释）。

用途：在给 AOX 加国际化之前，量清楚到底有多少条需要抽取的文案、分布在哪些文件。
仅作分析用，不修改任何文件。
"""
import re
import sys
import pathlib

CJK = re.compile(r'[\u4e00-\u9fff]')


def strip_comments_and_collect(path: pathlib.Path):
    """返回 (行号, 字符串字面量) 列表，已排除注释里的内容。"""
    src = path.read_text(encoding='utf-8', errors='replace')
    out = []
    i = 0
    n = len(src)
    line = 1
    in_block = False          # /* */ 深度（Rust 可嵌套）
    block_depth = 0
    while i < n:
        c = src[i]
        if c == '\n':
            line += 1
            i += 1
            continue
        if in_block:
            if src.startswith('/*', i):
                block_depth += 1
                i += 2
                continue
            if src.startswith('*/', i):
                block_depth -= 1
                if block_depth == 0:
                    in_block = False
                i += 2
                continue
            i += 1
            continue
        # 行注释
        if src.startswith('//', i):
            while i < n and src[i] != '\n':
                i += 1
            continue
        # 块注释
        if src.startswith('/*', i):
            in_block = True
            block_depth = 1
            i += 2
            continue
        # 字符字面量（跳过，避免误判 '{' 之类）
        if c == "'":
            # 可能是生命周期 'a 或字符 'x'；只在形如 'x' 或 '\n' 时跳过
            m = re.match(r"'(?:\\.|[^'\\])'", src[i:])
            if m:
                i += m.end()
                continue
            i += 1
            continue
        # 字符串字面量
        if c == '"':
            start_line = line
            j = i + 1
            buf = []
            while j < n:
                if src[j] == '\\':
                    buf.append(src[j:j + 2])
                    j += 2
                    continue
                if src[j] == '"':
                    break
                if src[j] == '\n':
                    line += 1
                buf.append(src[j])
                j += 1
            lit = ''.join(buf)
            if CJK.search(lit):
                out.append((start_line, lit))
            i = j + 1
            continue
        i += 1
    return out


def main():
    root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else '.')
    files = sorted(list(root.glob('src/**/*.rs')) + list(root.glob('gui/src/**/*.rs')))
    total = 0
    grand = 0
    for f in files:
        hits = strip_comments_and_collect(f)
        if hits:
            total += 1
            grand += len(hits)
            print(f'{len(hits):5d}  {f}')
    print(f'\n文件数 {total}，中文字符串字面量总数 {grand}')


if __name__ == '__main__':
    main()
