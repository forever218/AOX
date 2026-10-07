# -*- coding: utf-8 -*-
"""把源码里「面向用户的中文字面量」机械地包上 `t(...)` / `tf!(...)`。

这是**半自动**：脚本只做「结构上必然正确」的那几种改写，拿不准的一律跳过并报告，
由人收尾。判定依据是 `tools/check_i18n.py` 里那套扫描（同一份解析，不另起一套）。

改写规则（按优先级）：

| 原样 | 改成 | 说明 |
|---|---|---|
| `"中文"` | `t("中文")` | 普通实参 |
| `format!("中文 {}", x)` | `tf!("中文 {}", x)` | 只换宏名 |
| `println!("中文 {}", x)` | `println!("{}", tf!("中文 {}", x))` | 宏的**格式串必须是字面量**，所以套一层 |
| `println!("中文")` | `println!("{}", t("中文"))` | 同上，无参数时用 `t` |

跳过的（会在报告里列出来，需手改）：
  * 测试块里的（`#[cfg(test)]` / `mod tests`）
  * 已经包好的
  * 中文字面量是格式串、但**参数位置对不上**（例如 `println!("{}", "中文")` 里的第二个）
  * 出现在 `const` / 静态量 / 匹配模式里的（这些不能调用函数，得单独处理）

用法：
    python tools/apply_i18n.py --dry     # 只看会改哪些、跳过哪些
    python tools/apply_i18n.py           # 落盘改写
"""
from __future__ import annotations

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from check_i18n import ROOT, ignored, scan  # noqa: E402

# 模板是**第 1 个**实参的宏
WRAP_FIRST = {
    'println', 'print', 'eprintln', 'eprint', 'panic', 'anyhow', 'bail', 'ensure',
    'unreachable', 'todo', 'unimplemented', 'dbg',
}
# 模板是**第 2 个**实参的宏（第 1 个分别是 writer / 断言条件）
WRAP_SECOND = {'write', 'writeln', 'assert', 'debug_assert'}
# 模板是**第 3 个**实参
WRAP_THIRD = {'assert_eq', 'assert_ne', 'debug_assert_eq', 'debug_assert_ne'}


def build_mask(src: str):
    """标记「字符串 / 字符 / 注释」覆盖的下标，供结构字符（逗号、括号）的统计跳开它们。"""
    mask = bytearray(len(src))
    i, n = 0, len(src)
    block = 0
    while i < n:
        c = src[i]
        if block:
            mask[i] = 1
            if src.startswith('/*', i):
                mask[i + 1] = 1
                block += 1
                i += 2
            elif src.startswith('*/', i):
                mask[i + 1] = 1
                block -= 1
                i += 2
            else:
                i += 1
            continue
        if src.startswith('//', i):
            while i < n and src[i] != '\n':
                mask[i] = 1
                i += 1
            continue
        if src.startswith('/*', i):
            mask[i] = mask[i + 1] = 1
            block = 1
            i += 2
            continue
        if c == '"':
            j = i + 1
            while j < n:
                if src[j] == '\\':
                    mask[j] = mask[j + 1] = 1
                    j += 2
                    continue
                mask[j] = 1
                if src[j] == '"':
                    break
                j += 1
            mask[i] = 1
            i = j + 1
            continue
        i += 1
    return mask


def top_level_commas(src: str, mask, lo: int, hi: int) -> int:
    """[lo, hi) 里**顶层**逗号的个数（跳过字符串/注释、以及嵌套括号内的）。"""
    depth = 0
    count = 0
    i = lo
    while i < hi:
        if mask[i]:
            i += 1
            continue
        c = src[i]
        if c in '([{':
            depth += 1
        elif c in ')]}':
            depth -= 1
        elif c == ',' and depth == 0:
            count += 1
        i += 1
    return count


def matching_paren(src: str, mask, open_at: int) -> int:
    """`open_at` 是 `(` 的下标，返回配对的 `)` 的下标；找不到返回 -1。"""
    depth = 0
    i = open_at
    n = len(src)
    while i < n:
        if mask[i]:
            i += 1
            continue
        c = src[i]
        if c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def head_and_open(src: str, start: int) -> tuple[str, int]:
    k = start - 1
    while k >= 0 and src[k] in ' \t\r\n':
        k -= 1
    if k < 0 or src[k] != '(':
        return '', -1
    open_at = k
    k -= 1
    end = k
    while k >= 0 and (src[k].isalnum() or src[k] == '_'):
        k -= 1
    name = src[k + 1:end + 1]
    if k >= 0 and src[k] == '!':
        name += '!'
    return name, open_at


def main() -> int:
    dry = '--dry' in sys.argv
    files = sorted(list(ROOT.glob('src/**/*.rs')) + list(ROOT.glob('gui/src/**/*.rs')))

    changed_files = 0
    done, skipped = 0, []

    for path in files:
        with open(path, 'r', encoding='utf-8', newline='') as f:
            src = f.read()
        rel = path.relative_to(ROOT).as_posix()
        orig = src

        # 每次只改**一处**，改完重扫一遍。看着笨，但这样所有下标永远是当前源码的，
        # 不会出现「前一处插入的字符让后一处的偏移全错」那种最难查的错。
        while True:
            lits, tlines = scan_text(src, rel)
            lines = src.split('\n')
            head_done = False

            for lit in lits:
                if lit.line in tlines or lit.wrapped:
                    continue
                if 1 <= lit.line <= len(lines) and 'i18n-ignore' in lines[lit.line - 1]:
                    continue

                raw = lit.raw
                mask = build_mask(src)
                head, open_at = head_and_open(src, lit.start)

                if head == 'format!':
                    src = src[:open_at - 7] + 'tf!' + src[open_at:]
                    done += 1
                    head_done = True
                    break

                bare = head.rstrip('!')
                want = 0 if bare in WRAP_FIRST else 1 if bare in WRAP_SECOND else 2 if bare in WRAP_THIRD else None

                if want is None:
                    src = src[:lit.start] + 't(' + raw + ')' + src[lit.end + 1:]
                    done += 1
                    head_done = True
                    break

                # 格式宏：确认这个中文字面量**就是**它的格式串（而不是后面那些普通实参）
                if top_level_commas(src, mask, open_at + 1, lit.start) != want:
                    skipped.append((lit, f'{head} 的第 {want + 1} 个实参不是它'))
                    # 标成已处理，免得死循环：插一个空注释不行，改用「跳过表」按 (行, 起址) 记
                    skip_key.add((rel, lit.start))
                    continue
                close = matching_paren(src, mask, open_at)
                if close < 0:
                    skipped.append((lit, '括号配不上'))
                    skip_key.add((rel, lit.start))
                    continue
                rest = src[lit.end + 1:close]
                if rest.strip():
                    new = '"{}", tf!(' + raw + rest + ')'
                else:
                    new = '"{}", t(' + raw + ')'
                src = src[:lit.start] + new + src[lit.end + 1:]
                done += 1
                head_done = True
                break

            if not head_done:
                break

        if src != orig:
            changed_files += 1
            if not dry:
                with open(path, 'w', encoding='utf-8', newline='') as f:
                    f.write(src)

    print(f'{"[dry-run] " if dry else ""}改写 {done} 处，涉及 {changed_files} 个文件')
    if skipped:
        print(f'\n需手改 {len(skipped)} 处：')
        for lit, why in skipped:
            print(f'  {lit.where()}  ({why}): {lit.text[:60]}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
