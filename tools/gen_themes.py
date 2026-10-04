#!/usr/bin/env python
"""从 gui/src/theme.rs 的 LIGHT / DARK 常量生成 themes/paper.toml 与 themes/ink.toml。

为什么不手抄：这两天刚吃过「手工搬运长文本」的亏（阶段 1 的提示词、env_section）。
18 个颜色 × 2 套，抄错一个字节都不会有人发现，但主题文件是给用户当基准看的。
所以直接从源码里抽，抽完再反向核对一遍。
"""

import io
import re
import sys

SRC = "gui/src/theme.rs"

FIELD = re.compile(
    r"^\s*(\w+):\s*Color32::from_(rgb|rgba_unmultiplied_const)\(([^)]*)\),\s*$", re.M
)


def parse(name: str, text: str) -> list[tuple[str, str]]:
    m = re.search(
        r"const %s: Palette = Palette \{(.*?)\n\};" % name, text, re.S
    )
    if not m:
        raise SystemExit(f"ABORT: 在 {SRC} 里找不到 const {name}")
    out = []
    for fm in FIELD.finditer(m.group(1)):
        field, kind, args = fm.group(1), fm.group(2), fm.group(3)
        nums = [int(x.strip(), 0) for x in args.split(",")]
        if kind == "rgb":
            if len(nums) != 3:
                raise SystemExit(f"ABORT: {name}.{field} 分量数不对: {args}")
            out.append((field, "#%02X%02X%02X" % tuple(nums)))
        else:
            if len(nums) != 4:
                raise SystemExit(f"ABORT: {name}.{field} 分量数不对: {args}")
            out.append((field, "#%02X%02X%02X%02X" % tuple(nums)))
    if len(out) != 18:
        raise SystemExit(f"ABORT: {name} 只解析出 {len(out)} 个字段（应为 18）")
    return out


LEGEND = """\
# {title}
#
# 出厂{mode_cn}的**完整定义** —— 与内置配色逐字节相同，所以：
#   aox-gui --theme {path}
# 看到的画面应与不指定 --theme 时**完全一样**（可以用来验证主题文件确实被读进去了）。
#
# 改任意几行即可微调；只写想改的字段也行（其余继承内置）。
# 颜色写法："#RRGGBB" / "#RRGGBBAA" / "#RGB"，或数组 [r, g, b] / [r, g, b, a]。
#
# 本文件就是一份 `[theme]` 段，可以**整段粘进 config.toml**；
# 反过来，把 config.toml 里的 [theme] 整段摘出来也能存成主题文件用。
#
# 注意明暗：下面 mode 的优先级**低于**你手动点过的那次切换
# （记录在 <应用根>/.aox-gui-theme）；想让 mode 生效，先删掉那个文件。
"""


def emit(title: str, mode: str, path: str, pairs: list[tuple[str, str]]) -> str:
    head = LEGEND.format(
        title=title, path=path, mode_cn="纸（亮色）" if mode == "light" else "墨（暗色）"
    )
    body = "\n".join(f"{k} = \"{v}\"" for k, v in pairs)
    section = "light" if mode == "light" else "dark"
    return f'{head}\n[theme]\nmode = "{mode}"\n\n[theme.{section}]\n{body}\n'


def main() -> int:
    text = io.open(SRC, encoding="utf-8").read()
    light = parse("LIGHT", text)
    dark = parse("DARK", text)

    # 反向核对：抽出来的数组顺序应与 PaletteConfig 的字段顺序一致，
    # 否则合并时字段会串位（这是真正会出事的地方）。
    cfg = io.open("src/config.rs", encoding="utf-8").read()
    cm = re.search(r"pub struct PaletteConfig \{(.*?)\n\}", cfg, re.S)
    if not cm:
        raise SystemExit("ABORT: 找不到 PaletteConfig")
    cfg_fields = re.findall(r"pub (\w+):\s*Option<RgbColor>", cm.group(1))
    got = [k for k, _ in light]
    if cfg_fields != got:
        raise SystemExit(
            "ABORT: 字段顺序不一致\n  config.rs: %s\n  theme.rs : %s" % (cfg_fields, got)
        )
    print(f"字段顺序与 PaletteConfig 一致（{len(got)} 项）")

    files = {
        "themes/paper.toml": emit("纸 · paper", "light", "themes/paper.toml", light),
        "themes/ink.toml": emit("墨 · ink", "dark", "themes/ink.toml", dark),
    }
    for path, content in files.items():
        io.open(path, "w", encoding="utf-8", newline="\n").write(content)
        print(f"写入 {path}（{len(content)} 字节）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
