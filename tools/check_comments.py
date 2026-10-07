# -*- coding: utf-8 -*-
"""把注释全部剥掉，用来证明「这次改动只动了注释，代码一个字节没变」。

翻注释是**纯文本替换**，但它有个真实的风险：一句多行注释少写一个 `*/`、
或者把 `//` 写成 `///` 时顺带吃掉下一行代码。人眼在几千行里看不出来，
机器一秒钟就能比出来。

做法：把「git HEAD 的版本」和「工作区的版本」各自剥掉注释，逐字节比较。
剥法必须**两边同一条规则**：字符串里的 `//` 不算注释，注释里的引号不算字符串。

用法：
    python tools/check_comments.py                # 检查所有改动过的 .rs / .toml
    python tools/check_comments.py src/config.rs  # 只查指定文件

退出码：有文件「剥完注释后仍不一致」⇒ 1（说明代码被动了，要人工看）。
"""
from __future__ import annotations

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def _raw_string_at(src: str, i: int):
    """i 处若是 Rust 原始字符串的起点，返回**结束下标**（否则 None）。

    覆盖 `r"…"` / `r#"…"#` / `r##"…"##` / `br"…"` / `b"…"` 的兄弟形态。
    ⚠️ 少了这条，`r"\\?\\UNC\\"` 这类原始串里的反斜杠会被当成转义，
    把结尾引号一起吃掉，扫描器从那里起**整段失同步** —— 表现是文件里
    早就不存在的 `///` 注释突然出现在「剥完注释」的结果里（误报成代码被动过）。
    """
    j = i
    if src[j] == 'b':          # 可选的字节串前缀
        j += 1
    if j >= len(src) or src[j] != 'r':
        return None
    j += 1
    hashes = 0
    while j < len(src) and src[j] == '#':
        hashes += 1
        j += 1
    if j >= len(src) or src[j] != '"':
        return None
    closing = '"' + '#' * hashes
    k = src.find(closing, j + 1)
    if k < 0:
        return None
    return k + len(closing)


def _char_literal_at(src: str, i: int):
    """i 处若是 Rust 字符字面量（`'a'` / `'\\n'` / `'"'`），返回结束下标；生命周期 `'a` 返回 None。"""
    if i + 1 >= len(src):
        return None
    if src[i + 1] == '\\':     # 转义体：\n \t \' \\ \u{1F600} …
        j = i + 2
        if j < len(src) and src[j] == 'u':
            j += 1
            if j < len(src) and src[j] == '{':
                k = src.find('}', j)
                if k < 0:
                    return None
                j = k + 1
        else:
            j += 1
        return j + 1 if j < len(src) and src[j] == "'" else None
    if i + 2 < len(src) and src[i + 2] == "'":
        return i + 3
    return None


def strip_comments(src: str, hash_comment: bool = False,
                   semi_comment: bool = False) -> str:
    """去掉注释，保留其余字符（含换行），字符串字面量原样保留。

    `hash_comment` 用于 TOML/shell（`#`），`semi_comment` 用于 Inno Setup（`;`）。
    """
    out = []
    i, n = 0, len(src)
    block = 0
    while i < n:
        c = src[i]
        if block:
            if src.startswith('/*', i):
                block += 1
                i += 2
            elif src.startswith('*/', i):
                block -= 1
                i += 2
            elif c == '\n':
                out.append('\n')      # 保留行结构，两边才对得齐
                i += 1
            else:
                i += 1
            continue
        if src.startswith('//', i):
            while i < n and src[i] != '\n':
                i += 1
            continue
        if src.startswith('/*', i):
            block = 1
            i += 2
            continue
        if hash_comment and c == '#':
            while i < n and src[i] != '\n':
                i += 1
            continue
        if semi_comment and c == ';':
            while i < n and src[i] != '\n':
                i += 1
            continue
        if c in 'rb':
            end = _raw_string_at(src, i)
            if end is not None:
                out.append(src[i:end])     # 原始串整体保留（里面的 // 与引号都不是语法）
                i = end
                continue
        if c == "'":
            end = _char_literal_at(src, i)
            if end is not None:
                out.append(src[i:end])
                i = end
                continue
        if c == '"':
            out.append(c)
            i += 1
            while i < n:
                if src[i] == '\\':
                    out.append(src[i:i + 2])
                    i += 2
                    continue
                out.append(src[i])
                if src[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        out.append(c)
        i += 1
    return ''.join(out)


def head_version(path: str) -> str | None:
    r = subprocess.run(['git', 'show', f'HEAD:{path}'], cwd=ROOT,
                       capture_output=True)
    if r.returncode != 0:
        return None
    return r.stdout.decode('utf-8', 'replace')


def changed_files() -> list[str]:
    r = subprocess.run(['git', 'status', '--porcelain'], cwd=ROOT, capture_output=True, text=True)
    out = []
    for line in r.stdout.splitlines():
        status, _, path = line[:2], line[2:], line[3:].strip()
        if not path.endswith(('.rs', '.toml', '.iss')):
            continue
        out.append(path.strip('"'))
    return out


def code_lines(src: str, hash_comment: bool = False,
               semi_comment: bool = False) -> list[str]:
    """剥掉注释后，只留「非空行」。

    ⚠️ 不能拿「剥完的整段文本按行比」：翻注释时**注释行数一定会变**
    （中文 3 行 → 英文 5 行），而 `strip_comments` 把注释行变成「没有 \\r 的空行」、
    把真正的空行留成「带 \\r 的空行」——两种空行在行号上一错位，整表就报假警
    （实测 3 个文件全中招）。真正该问的是「代码行有没有变」，所以空行一律不参与比较。
    """
    stripped = strip_comments(src, hash_comment, semi_comment)
    return [ln for ln in stripped.splitlines() if ln.strip()]


def first_diff(al: list[str], bl: list[str]) -> str:
    for i in range(max(len(al), len(bl))):
        x = al[i] if i < len(al) else '<缺行>'
        y = bl[i] if i < len(bl) else '<缺行>'
        if x != y:
            return f'第 {i + 1} 个代码行：\n    HEAD: {x.strip()[:110]}\n    现在: {y.strip()[:110]}'
    return '（代码行数一致但内容不同）'


def main() -> int:
    targets = sys.argv[1:] or changed_files()
    bad = 0
    for path in targets:
        p = ROOT / path
        if not p.exists():
            print(f'  --   {path}（工作区里没有，跳过）')
            continue
        old = head_version(path)
        if old is None:
            print(f'  ..   {path}（HEAD 里没有，新文件）')
            continue
        # ⚠️ 必须 newline='' —— 默认的通用换行会把 CRLF 折成 LF，
        # 于是「剥掉注释后」两边永远差一个 \r，全表报假警（这个坑实测踩到过）。
        with open(p, 'r', encoding='utf-8', errors='replace', newline='') as f:
            new = f.read()
        hc = path.endswith('.toml')
        sc = path.endswith('.iss')
        a, b = code_lines(old, hc, sc), code_lines(new, hc, sc)
        if a == b:
            print(f'  OK   {path}')
        else:
            bad += 1
            print(f'  ❌   {path} —— 剥掉注释后代码仍不一致：')
            print('       ' + first_diff(a, b))

    print(f'\n{len(targets)} 个文件，{bad} 个「代码被动过」')
    return 1 if bad else 0


if __name__ == '__main__':
    sys.exit(main())
