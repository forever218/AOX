# -*- coding: utf-8 -*-
"""i18n 覆盖率检查 —— 「英文版一个中文都不漏」的可验证依据。

三件事：
  1. **漏网**：源码里还有哪些中文字面量没被 `t(...)` / `tf!(...)` 包住（排除测试块）。
  2. **缺项**：被包住了，但 `locales/en.toml` 里没有对应键。
  3. **多余的项**：词典里有、源码里已经找不到的键（通常是中文原文被改过）。

用法：
    python tools/check_i18n.py            # 只看摘要 + 漏网清单
    python tools/check_i18n.py -v         # 连缺项 / 多余项也逐条列出
    python tools/check_i18n.py --gen      # 把缺的键打印成可直接粘进 en.toml 的空壳

退出码：有漏网或缺项 ⇒ 1（可以接进 CI / 构建前的闸门）。

被当作**数据**（而不是文案）的中文字面量，加 `// i18n-ignore` 注释在同一行即可豁免 ——
比如用来跟用户输入比较、或者协议里约定的固定字符串。豁免要写在一眼能看见的地方，
免得日后有人照着抄。
"""
from __future__ import annotations

import pathlib
import re
import sys
import tomllib

CJK = re.compile(r'[\u4e00-\u9fff]')
ROOT = pathlib.Path(__file__).resolve().parent.parent
CATALOG = ROOT / 'locales' / 'en.toml'


# ───────────────────────── Rust 源码的极简扫描 ─────────────────────────

class Lit:
    __slots__ = ('path', 'line', 'text', 'start', 'end', 'raw', 'wrapped', 'is_format')

    def __init__(self, path, line, text, start, end, raw, wrapped, is_format):
        self.path, self.line, self.text, self.start, self.end = path, line, text, start, end
        self.raw, self.wrapped, self.is_format = raw, wrapped, is_format

    def where(self):
        return f'{self.path}:{self.line}'


def scan(path: pathlib.Path) -> tuple[list[Lit], set[int]]:
    """读文件并扫描。"""
    src = path.read_text(encoding='utf-8', errors='replace')
    return scan_text(src, path.relative_to(ROOT).as_posix())


def scan_text(src: str, rel: str) -> tuple[list[Lit], set[int]]:
    """返回 (中文字面量列表, 落在测试块里的行号集合)。

    只认字符串本身，注释里的中文一律跳过 —— 注释是给开发者看的，不进二进制。
    """
    lines = src.split('\n')

    lits: list[Lit] = []
    i, n, line = 0, len(src), 1
    block = 0  # /* */ 嵌套深度

    while i < n:
        c = src[i]

        if c == '\n':
            line += 1
            i += 1
            continue

        # 块注释
        if block:
            if src.startswith('/*', i):
                block += 1
                i += 2
            elif src.startswith('*/', i):
                block -= 1
                i += 2
            else:
                i += 1
            continue

        # 行注释
        if src.startswith('//', i):
            while i < n and src[i] != '\n':
                i += 1
            continue
        if src.startswith('/*', i):
            block = 1
            i += 2
            continue

        # 字符字面量 / 生命周期
        if c == "'":
            m = re.match(r"'(?:\\.|[^'\\])'", src[i:])
            i += m.end() if m else 1
            continue

        # 原始字符串 r"..." / r#"..."# / br#"..."#
        m = re.match(r'(?:b?r)(#*)"', src[i:])
        if m:
            hashes, j = m.group(1), i + m.end()
            end = '"' + hashes
            k = src.find(end, j)
            k = n if k < 0 else k + len(end)
            line += src.count('\n', i, k)
            lit = src[j:k - len(end)]
            _record(lits, src, rel, line, lit, i, k, src[i:k])
            i = k
            continue

        # 普通字符串 b"..." / "..."
        m = re.match(r'b?"', src[i:])
        if m:
            quote_at = i + m.end() - 1
            start_line = line
            j = quote_at + 1
            buf = []
            while j < n:
                if src[j] == '\\':
                    # 行尾的反斜杠是**续行**：它把换行也吃掉了，计数不能漏
                    if j + 1 < n and src[j + 1] == '\n':
                        line += 1
                    buf.append(src[j:j + 2])
                    j += 2
                    continue
                if src[j] == '"':
                    break
                if src[j] == '\n':
                    line += 1
                buf.append(src[j])
                j += 1
            _record(lits, src, rel, start_line, ''.join(buf), i, j, src[i:j + 1])
            i = j + 1
            continue

        i += 1

    return lits, test_lines(lines)


def call_open(src: str, start: int) -> int:
    """字面量左边那个 `(` 的下标；不是「直接作为某个调用/宏的实参」则返回 -1。"""
    k = start - 1
    while k >= 0 and src[k] in ' \t\r\n':
        k -= 1
    return k if k >= 0 and src[k] == '(' else -1


def call_head(src: str, start: int) -> str:
    """字面量左边那个「调用头」的名字：`t(` → `t`，`tf!(` → `tf!`，`on_hover_text(` → `on_hover_text`。

    为什么要看整个标识符而不是只看前两个字符：`on_hover_text("…")`、`expect("…")`
    这种「以 `t(` 结尾的长名字」会被朴素的前缀判断误认成已经包好了 —— 实测踩到过，
    几十条漏网瞬间从清单里消失。
    """
    open_at = call_open(src, start)
    if open_at < 0:
        return ''
    k = open_at - 1
    end = k
    while k >= 0 and (src[k].isalnum() or src[k] == '_'):
        k -= 1
    name = src[k + 1:end + 1]
    if k >= 0 and src[k] == '!':
        name += '!'
    return name


def _record(lits, src, rel, line, lit, start, end, raw):
    if not CJK.search(lit):
        return
    head = call_head(src, start)
    wrapped = head in ('t', 'tf!')
    is_format = head == 'format!'
    lits.append(Lit(rel, line, lit, start, end, raw, wrapped, is_format))


def test_lines(lines: list[str]) -> set[int]:
    """`#[cfg(test)]` / `mod tests` 块覆盖的行号（1 基）。"""
    marked: set[int] = set()

    def block_end(start: int) -> int:
        depth, seen = 0, False
        for i in range(start, len(lines)):
            depth += lines[i].count('{') - lines[i].count('}')
            seen = seen or '{' in lines[i]
            if seen and depth <= 0:
                return i
        return len(lines) - 1

    for i, ln in enumerate(lines):
        s = ln.strip()
        if s.startswith('mod tests') or s.startswith('#[cfg(test)]'):
            for k in range(i, block_end(i) + 1):
                marked.add(k + 1)
    return marked


def ignored(path: pathlib.Path, line: int) -> bool:
    text = path.read_text(encoding='utf-8', errors='replace').split('\n')
    if not (1 <= line <= len(text)):
        return False
    return 'i18n-ignore' in text[line - 1]


# ───────────────────────── 词典 ─────────────────────────

def load_catalog() -> dict[str, str]:
    if not CATALOG.exists():
        return {}
    with open(CATALOG, 'rb') as f:
        return tomllib.load(f)


# ───────────────────────── 主流程 ─────────────────────────

def main() -> int:
    verbose = '-v' in sys.argv or '--verbose' in sys.argv
    gen = '--gen' in sys.argv

    files = sorted(list(ROOT.glob('src/**/*.rs')) + list(ROOT.glob('gui/src/**/*.rs')))
    catalog = load_catalog()

    leaks: list[Lit] = []       # 没包住的（且不在测试里）
    unwrapped_fmt: list[Lit] = []  # 用 format! 当模板的（要改 tf!）
    missing: list[Lit] = []     # 包住了但词典没有
    keys_used: set[str] = set()
    test_hits = 0

    for f in files:
        lits, tlines = scan(f)
        for lit in lits:
            if lit.line in tlines:
                test_hits += 1
                continue
            if ignored(ROOT / lit.path, lit.line):
                continue
            if lit.wrapped:
                keys_used.add(unescape(lit.text))
                if unescape(lit.text) not in catalog:
                    missing.append(lit)
            else:
                leaks.append(lit)
                if lit.is_format:
                    unwrapped_fmt.append(lit)

    print(f'== i18n 覆盖率 ==')
    print(f'  待翻译文件            {len(files)}')
    print(f'  测试块里的中文（跳过）  {test_hits}')
    print(f'  词典条目              {len(catalog)}')
    print(f'  已包住且词典有      {len(keys_used & set(catalog))}')
    print(f'  ❌ 漏网（未包 t/tf）   {len(leaks)}')
    print(f'  ❌ 缺项（词典没有）    {len(missing)}')

    stale = sorted(set(catalog) - keys_used)
    print(f'  ⚠ 多余（源码已无）    {len(stale)}')

    if leaks:
        print(f'\n-- 漏网（未包住的中文字面量）--')
        for lit in leaks:
            tag = '  [format! → 应改 tf!]' if lit.is_format else ''
            print(f'  {lit.where()}{tag}: {one_line(lit.text)}')

    if gen:
        print('\n-- 缺的词典条目（粘进 locales/en.toml，填上英文即可）--')
        seen = set()
        for lit in missing + leaks:
            k = unescape(lit.text)
            if k in seen:
                continue
            seen.add(k)
            print(f'{toml_key(k)} = ""')

    if verbose and missing:
        print('\n-- 缺项（词典里没有）--')
        for lit in missing:
            print(f'  {lit.where()}: {one_line(lit.text)}')

    if verbose and stale:
        print('\n-- 多余（词典里有、源码里没有）--')
        for k in stale:
            print(f'  {toml_key(k)}')

    return 1 if (leaks or missing) else 0


def unescape(s: str) -> str:
    """把扫描到的原始字面量还原成真正的内容（反解 \\n \\" \\\\ 等）。"""
    out, i = [], 0
    while i < len(s):
        if s[i] == '\\' and i + 1 < len(s):
            nxt = s[i + 1]
            out.append({'n': '\n', 't': '\t', 'r': '\r', '"': '"', '\\': '\\', '0': '\0'}.get(nxt, nxt))
            i += 2
            continue
        out.append(s[i])
        i += 1
    return ''.join(out)


def toml_key(k: str) -> str:
    """输出一个 TOML 基本字符串键（保证能直接粘进词典）。"""
    esc = k.replace('\\', '\\\\').replace('"', '\\"').replace('\n', '\\n').replace('\t', '\\t')
    return f'"{esc}"'


def one_line(s: str, limit: int = 90) -> str:
    s = s.replace('\n', '\\n')
    return s if len(s) <= limit else s[:limit] + '…'


if __name__ == '__main__':
    sys.exit(main())
