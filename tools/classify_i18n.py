# -*- coding: utf-8 -*-
"""在中文字面量基础上做分类：test / prompt / product。

分类规则（粗但够用）：
- 位于 `#[cfg(test)]` 或 `mod tests { ... }` 块内 → test（开发者可见，不必翻译）
- 其余按文件归到 product（界面/错误提示）或 prompt（给模型的提示词、工具描述）

只读，不改文件。
"""
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from scan_i18n import strip_comments_and_collect

root = pathlib.Path(__file__).parent.parent

PROMPT_FILES = {
    'src/prompt.rs', 'src/tool/meta.rs', 'src/tool/hostfs.rs', 'src/tool/ask.rs',
    'src/plugin/mod.rs', 'src/selftest.rs', 'src/auth.rs',
}


def test_line_ranges(path: pathlib.Path):
    """返回落在测试块内的行号集合。用括号配平从 `mod tests` / `#[cfg(test)]` 起算。"""
    src = path.read_text(encoding='utf-8', errors='replace')
    lines = src.split('\n')
    marked = set()

    def block_from(start_idx):
        """从 start_idx 行开始配平花括号，返回结束行号（含）。"""
        depth = 0
        seen = False
        for i in range(start_idx, len(lines)):
            for ch in lines[i]:
                if ch == '{':
                    depth += 1
                    seen = True
                elif ch == '}':
                    depth -= 1
            if seen and depth <= 0:
                return i
        return len(lines) - 1

    for i, ln in enumerate(lines):
        s = ln.strip()
        if s.startswith('mod tests') or s.startswith('#[cfg(test)]'):
            end = block_from(i)
            for k in range(i, end + 1):
                marked.add(k + 1)
    return marked


def main():
    files = sorted(list(root.glob('src/**/*.rs')) + list(root.glob('gui/src/**/*.rs')))
    tally = {'product': 0, 'prompt': 0, 'test': 0}
    per_file = {}
    tpl = 0
    for f in files:
        rel = f.relative_to(root).as_posix()
        hits = strip_comments_and_collect(f)
        if not hits:
            continue
        tlines = test_line_ranges(f)
        c = {'product': 0, 'prompt': 0, 'test': 0}
        for ln, lit in hits:
            if ln in tlines:
                c['test'] += 1
            elif rel in PROMPT_FILES:
                c['prompt'] += 1
            else:
                c['product'] += 1
            if '{}' in lit or re.search(r'\{[a-z_]', lit):
                tpl += 1
        for k in c:
            tally[k] += c[k]
        per_file[rel] = c
    print(f'{"文件":44s} product prompt  test')
    for rel, c in sorted(per_file.items(), key=lambda kv: -(kv[1]['product'] + kv[1]['prompt'])):
        print(f'{rel:44s} {c["product"]:7d} {c["prompt"]:6d} {c["test"]:5d}')
    print(f'\n合计 product={tally["product"]}  prompt={tally["prompt"]}  test={tally["test"]}')
    print(f'含占位符（模板）的字面量：{tpl}')


if __name__ == '__main__':
    main()
