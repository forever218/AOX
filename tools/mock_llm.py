#!/usr/bin/env python3
"""极简 OpenAI 兼容 mock 端点（仅用于 GUI 端到端验证）。

按「已出现的 tool 消息条数」分阶段返回，模拟一个思考模型：
  阶段 0：推理过程 + 调用 list_plugins（元工具 → 触发 GUI 授权弹窗）
  阶段 1：推理过程 + 调用 fs_read_file
  阶段 2：Markdown 最终答复

用法: python tools/mock_llm.py [port]
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8791

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
        self._chunk({
            "choices": [],
            "usage": {"prompt_tokens": 128, "completion_tokens": 256,
                      "total_tokens": 384, "completion_tokens_details": {"reasoning_tokens": 96}},
        })
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0) or 0)
        try:
            body = json.loads(self.rfile.read(n) or b"{}")
        except Exception:
            body = {}
        msgs = body.get("messages", [])
        tool_msgs = sum(1 for m in msgs if m.get("role") == "tool")

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        try:
            if tool_msgs == 0:
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
