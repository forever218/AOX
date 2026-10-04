#!/usr/bin/env python3
"""AOX 示例组件（阶段 6 · 6b）—— 除工具之外，还贡献钩子与提示词段。

怎么用起来
    把这个目录整个拷到**应用根下的 plugins/** 里，重启 aox 即可。
    （留在 tools/samples/ 下是不会生效的 —— 宿主只扫描 plugins/。）

看效果
    /plugins   插件清单下面会多出一行「组件贡献：…」
    /prompt    系统提示里会多出 house_rules 这一段，来源标「插件」
    /hook      钩子列表里会出现本组件（含它申请的能力、以及失败计数）

设计要点（值得照抄的地方）
    · 钩子**不许**做耗时的事：它挡着主循环，超过 10 秒就按「组件坏了」处理。
    · 钩子抛异常不会连累宿主 —— serialize 层会写 stderr 并按「放行」返回。
    · 「只做一次」这类防重复逻辑要写在组件自己这边（见 _injected）。
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from plugin_sdk import serve  # noqa: E402

# 用来演示「防重复是组件自己的责任」：注入过一次就不再注入。
_injected = False


def check(args):
    """一个平凡的工具。

    它存在的意义是给 [contributes] caps 一个**作用对象**：声明了 caps 之后，
    调用它会让宿主弹一次授权询问（「你允许这个组件动文件吗」）。
    """
    return "示例组件在线：钩子与提示词段都已挂载。"


def before_request(params):
    """请求前：要往对话里塞话，就塞在这里。

    这个钩子点只开放**追加**（而且只能是用户消息）—— 想改历史请用
    config.toml 的 [loop] max_rounds，那是唯一一条能同时改内存与对话文件的路径。
    """
    global _injected
    if _injected:
        return {"verdict": "pass"}
    _injected = True
    return {
        "verdict": "pass",
        "inject": ["（组件 house_rules）这段对话我打过标记了，按结论说话就行。"],
    }


def after_tool(params):
    """工具结果后：给结果盖个戳 —— 证明组件真的改到了执行路径上。"""
    if params.get("is_error"):
        # 出错的结果不动它，免得把诊断信息淹掉
        return {"verdict": "pass"}
    return {"content": params.get("content", "") + "\n\n（本行由示例组件 house_rules 追加）"}


serve(
    "house_rules",
    {
        "house_rules_check": (
            "示例组件提供的平凡工具（用来试能力申请）",
            {"type": "object", "properties": {}},
            check,
        ),
    },
    hooks={"before_request": before_request, "after_tool": after_tool},
    prompt_segments={
        "rules": (
            "## 本项目的几条规矩\n\n"
            "1. 改代码前先说清打算动哪几个文件、为什么。\n"
            "2. 不要顺手重构与本次任务无关的部分。\n"
            "3. 结论要给可验证的证据（命令、输出、文件路径）。\n\n"
            "（这一段由组件 house_rules 贡献 —— 见 plugins/house_rules/plugin.toml 的 [contributes]）"
        )
    },
)
