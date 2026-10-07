# -*- coding: utf-8 -*-
"""把 scan_i18n 的结果连同位置导成一份可读报告。"""
import sys
import pathlib

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from scan_i18n import strip_comments_and_collect

root = pathlib.Path(__file__).parent.parent
files = sorted(list(root.glob('src/**/*.rs')) + list(root.glob('gui/src/**/*.rs')))
out = root / 'tools' / 'i18n_report.txt'
with open(out, 'w', encoding='utf-8') as o:
    for f in files:
        hits = strip_comments_and_collect(f)
        if not hits:
            continue
        rel = f.relative_to(root)
        o.write(f'\n===== {rel} ({len(hits)}) =====\n')
        for ln, lit in hits:
            s = lit.replace('\n', '\\n')
            if len(s) > 200:
                s = s[:200] + '…'
            o.write(f'  {rel}:{ln}: {s}\n')
print('written', out)
