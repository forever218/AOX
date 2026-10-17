#!/usr/bin/env python3
"""极简 OpenAI 兼容 mock 端点（仅用于 GUI 端到端验证）。

按「已出现的 tool 消息条数」分阶段返回，模拟一个思考模型：
  阶段 0：推理过程 + 调用 list_plugins（元工具 → 触发 GUI 授权弹窗）
  阶段 1：推理过程 + 调用 fs_read_file
  阶段 2：Markdown 最终答复

用法: python tools/mock_llm.py [port]

环境变量 `AOX_MOCK_SCENARIO=ask` 换一套剧本，专门验 2026-10-06 加的三个界面能力
（选项槽 / 图片显示 / SVG 渲染）—— 它们都必须在**真链路**里验，因为没有模型就
没人会调 `ask_user`、没人会挂图、没人会写 SVG：

  阶段 0：`ask_user`（问一句 + 三个选项）  → 界面出选项槽，等用户点
  阶段 1：`attach_file` 挂一张本机 PNG     → 图片必须真的显示在对话里
  阶段 2：最终答复里带一个 ```svg 围栏     → 必须画出来，而不是显示成代码

图片路径由 `AOX_MOCK_IMAGE` 指定（默认取 `%TEMP%/aox-mock-demo.png`），
用 `tools/make_test_png.py` 生成一张带红蓝两块的图 —— 红蓝位置能一眼看出
「通道顺序有没有搞反」。
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8791

# 上一轮请求的输入规模 —— 前缀缓存的真实语义就是「这一轮复用的是上一轮那整段前缀」。
# 用它（而不是写死的数字）算 usage，缓存命中才会随着上下文变长而一直往前走；
# 写死的 128/64 会让「缓存命中图」永远停在同一个位置，等于把这个窗口最该验的东西验掉了。
LAST_PROMPT = [0]

SCENARIO = os.environ.get("AOX_MOCK_SCENARIO", "").strip().lower()


# ── ask 剧本：选项槽 / 图片显示 / SVG 渲染，一条链走完 ──
#
# 这三件事都必须在**真链路**里验：没有模型，就没人会调 `ask_user`、没人会挂图、
# 没人会写 SVG。剧本由 `AOX_MOCK_SCENARIO=ask` 选中。

MOCK_IMAGE = os.environ.get("AOX_MOCK_IMAGE") or os.path.join(
    os.environ.get("TEMP", "/tmp"), "aox-mock-demo.png"
)

ASK_QUESTION = "全盘扫描有两种口径，你要哪一种？"

ASK_OPTIONS = [
    "先扫目录、只统计文件数量（快，几秒）",
    "连每个文件大小一起统计（慢一些，但能出面积图）",
    "只统计我自己指定的几个目录",
]

ASK_FINAL = """路线定了，我按你选的做。

## 调用链

```svg
<svg viewBox="0 0 520 210">
  <rect x="10" y="20" width="130" height="46" rx="8" fill="#e8f0fe" stroke="#3b6ea5" stroke-width="2"/>
  <text x="75" y="48" font-size="15" text-anchor="middle" fill="#1a3a5c">用户输入</text>
  <line x1="140" y1="43" x2="196" y2="43" stroke="#3b6ea5" stroke-width="2"/>
  <line x1="196" y1="43" x2="186" y2="38" stroke="#3b6ea5" stroke-width="2"/>
  <line x1="196" y1="43" x2="186" y2="48" stroke="#3b6ea5" stroke-width="2"/>
  <rect x="196" y="20" width="150" height="46" rx="8" fill="#e8f0fe" stroke="#3b6ea5" stroke-width="2"/>
  <text x="271" y="48" font-size="15" text-anchor="middle" fill="#1a3a5c">宿主主循环</text>
  <line x1="346" y1="43" x2="402" y2="43" stroke="#3b6ea5" stroke-width="2"/>
  <line x1="402" y1="43" x2="392" y2="38" stroke="#3b6ea5" stroke-width="2"/>
  <line x1="402" y1="43" x2="392" y2="48" stroke="#3b6ea5" stroke-width="2"/>
  <rect x="402" y="20" width="108" height="46" rx="8" fill="#e8f0fe" stroke="#3b6ea5" stroke-width="2"/>
  <text x="456" y="48" font-size="15" text-anchor="middle" fill="#1a3a5c">模型</text>
  <rect x="196" y="120" width="150" height="46" rx="8" fill="#fdf2e3" stroke="#b8860b" stroke-width="2"/>
  <text x="271" y="148" font-size="15" text-anchor="middle" fill="#7a5800">工具调用</text>
  <line x1="271" y1="66" x2="271" y2="120" stroke="#b8860b" stroke-width="2"/>
  <line x1="271" y1="120" x2="266" y2="110" stroke="#b8860b" stroke-width="2"/>
  <line x1="271" y1="120" x2="276" y2="110" stroke="#b8860b" stroke-width="2"/>
</svg>
```

上面那张 SVG 是**画出来的**（不是代码块）—— 它验的是三种形状（rect / line / text）
与两个属性（fill / stroke），箭头是两条 `<line>` 拼出来的。

图里的红蓝两块如果左右调了个，说明 GDI+ 那步 BGRA→RGBA 的通道顺序错了。"""


PHASE0_REASON = (
    "用户想知道现在有哪些能力可用。我应该先 list_plugins 看看已加载的插件，"
    "再决定是否需要自己造一个新的。注意这是元工具，宿主会向用户请求授权。"
)
PHASE1_REASON = (
    "已经拿到插件清单了。接下来我读一下 config.toml，确认当前用的模型和端点，"
    "这样回答里可以带上具体配置。"
)
FINAL = """已经看完了宿主的现状，结论如下：

## 当前能力

- 元工具：`create_plugin`、`list_plugins`、`remove_plugin`
- 已加载插件：见右侧 **活动流** 面板中的记录

## 关于配置文件

```toml
[provider]
base_url = "http://127.0.0.1:8791/v1"
model    = "mock-model"
```

> 这条回复来自本地的 mock 端点，用来验证 GUI 的流式渲染、折叠与活动流。

| 环节 | 状态 |
|------|------|
| 授权 | 已通过 |
| 工具调用 | 完成 |
| 渲染 | 正常 |

需要我继续造一个新插件吗？"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _chunk(self, obj):
        data = "data: " + json.dumps(obj, ensure_ascii=False) + "\n\n"
        self.wfile.write(data.encode("utf-8"))
        self.wfile.flush()

    def _delta(self, d, finish=None):
        self._chunk({"choices": [{"index": 0, "delta": d, "finish_reason": finish}]})

    def _reason(self, text):
        for i in range(0, len(text), 8):
            self._delta({"reasoning_content": text[i:i + 8]})
            time.sleep(0.04)

    def _tool_call(self, idx, call_id, name, args):
        self._delta({
            "tool_calls": [{
                "index": idx,
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": args},
            }]
        })

    def _end(self, finish):
        self._delta({}, finish=finish)
        # usage 要给全：界面上的 token 明细（输入/输出/合计/推理/缓存）全靠它。
        # cached + cache_miss = prompt，别写成对不上账的数。
        #
        # cached 取**上一轮**的 prompt：这就是前缀缓存真实的样子 —— 这一轮复用的是上一轮那整段
        # 前缀，多出来的是这一轮新加的内容（工具结果、助手回复），必须重新算。输入规模按消息正文
        # 的字符数估，和宿主自己的口径一致（4 个 ASCII 字符 ≈ 1 token）。
        prompt = getattr(self, "_prompt", 128)
        cached = min(LAST_PROMPT[0], prompt)
        LAST_PROMPT[0] = prompt
        self._chunk({
            "choices": [],
            "usage": {"prompt_tokens": prompt, "completion_tokens": 256,
                      "total_tokens": prompt + 256,
                      "prompt_tokens_details": {"cached_tokens": cached},
                      "prompt_cache_miss_tokens": prompt - cached,
                      "completion_tokens_details": {"reasoning_tokens": 96}},
        })
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def _ask_scenario(self, tool_msgs):
        """阶段 0 问选项、阶段 1 挂图、阶段 2 出带 SVG 的答复。"""
        if tool_msgs == 0:
            self._reason(
                "用户要的是全盘扫描，但口径有两种，收益差很远。我不该在正文里写"
                "「你选 1 还是 2」—— 那是纯文本，宿主接不住，用户只能自己打字。"
                "用 ask_user 让他点一下。"
            )
            args = json.dumps(
                {"question": ASK_QUESTION, "options": ASK_OPTIONS}, ensure_ascii=False
            )
            self._tool_call(0, "call_ask", "ask_user", args)
            self._end("tool_calls")
        elif tool_msgs == 1:
            self._reason("他选好了。接下来把扫描结果图挂给他看。")
            args = json.dumps({"path": MOCK_IMAGE}, ensure_ascii=True)
            self._tool_call(0, "call_img", "attach_file", args)
            self._end("tool_calls")
        else:
            for i in range(0, len(ASK_FINAL), 10):
                self._delta({"content": ASK_FINAL[i:i + 10]})
                time.sleep(0.015)
            self._end("stop")

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0) or 0)
        try:
            body = json.loads(self.rfile.read(n) or b"{}")
        except Exception:
            body = {}
        msgs = body.get("messages", [])
        tool_msgs = sum(1 for m in msgs if m.get("role") == "tool")
        # 这一轮的输入规模：按整份请求正文估，宿主那套 token 明细才有真东西可显示。
        chars = sum(len(json.dumps(m, ensure_ascii=False)) for m in msgs)
        self._prompt = max(1, chars // 4)

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        try:
            if SCENARIO == "ask":
                self._ask_scenario(tool_msgs)
            elif tool_msgs == 0:
                self._reason(PHASE0_REASON)
                self._tool_call(0, "call_1", "list_plugins", "{}")
                self._end("tool_calls")
            elif tool_msgs == 1:
                self._reason(PHASE1_REASON)
                self._tool_call(0, "call_2", "fs_read_file", '{"path":"config.toml"}')
                self._end("tool_calls")
            else:
                for i in range(0, len(FINAL), 12):
                    self._delta({"content": FINAL[i:i + 12]})
                    time.sleep(0.02)
                self._end("stop")
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"mock LLM on http://127.0.0.1:{PORT}/v1", flush=True)
    srv.serve_forever()
