# -*- coding: utf-8 -*-
"""把改动过的文件换行「修回原样」，避免翻注释时顺手把整份文件的换行改了。

这个仓库**没有 .gitattributes，core.autocrlf=false**，同一目录里既有 CRLF 也有 LF
（`gui/src/tex.rs` 是 LF，`src/tool/mod.rs` 是 CRLF）。一旦某个文件的换行被整体翻转，
git diff 会把**每一行**都算成改动 —— 实测踩到过一份 3784 行的全文件 diff。

**判定基准永远是 git HEAD**（不是「哪个更常见」）：HEAD 是 CRLF 就修成 CRLF，
是 LF 就修成 LF。HEAD 里没有的（新文件）默认 LF，并单独列出来让人确认。

用法：
    python tools/fix_newlines.py                # 处理所有改动过的 .rs / .toml
    python tools/fix_newlines.py gui/src/tex.rs # 只处理指定文件
    python tools/fix_newlines.py --check        # 只报告、不改动
"""
from __future__ import annotations

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def head_bytes(path: str) -> bytes | None:
    r = subprocess.run(['git', 'show', f'HEAD:{path}'], cwd=ROOT, capture_output=True)
    return r.stdout if r.returncode == 0 else None


def to_lf(b: bytes) -> bytes:
    return b.replace(b'\r\n', b'\n').replace(b'\r', b'\n')


def apply_style(b: bytes, crlf: bool) -> bytes:
    lf = to_lf(b)
    return lf.replace(b'\n', b'\r\n') if crlf else lf


def changed_files() -> list[str]:
    r = subprocess.run(['git', 'status', '--porcelain'], cwd=ROOT,
                       capture_output=True, text=True)
    out = []
    for line in r.stdout.splitlines():
        path = line[3:].strip().strip('"')
        if path.endswith(('.rs', '.toml')):
            out.append(path)
    return out


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    check_only = '--check' in sys.argv
    targets = args or changed_files()
    bad = 0
    for path in targets:
        p = ROOT / path
        if not p.exists():
            print(f'  --   {path}（工作区里没有，跳过）')
            continue
        hb = head_bytes(path)
        cur = p.read_bytes()
        if hb is None:
            print(f'  ++   {path}（HEAD 里没有 = 新文件；默认 LF）')
            want = False
        else:
            want = b'\r\n' in hb
        cur_crlf = b'\r\n' in cur
        if cur_crlf == want:
            print(f'  OK   {path}（{"CRLF" if want else "LF"}）')
            continue
        bad += 1
        print(f'  FIX  {path}  {"CRLF" if cur_crlf else "LF"} -> {"CRLF" if want else "LF"}')
        if not check_only:
            p.write_bytes(apply_style(cur, want))
    print(f'\n{len(targets)} 个文件，{bad} 个换行不对'
          + ('（--check 只报告）' if check_only else '（已修正）'))
    return 1 if bad and check_only else 0


if __name__ == '__main__':
    sys.exit(main())
