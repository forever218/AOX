# aox-gui

AOX 的图形界面前端。它**不是**一个新实现，而是既有核心库的一个「视图 + 输入」层。

## 为什么能零分叉

`src/lib.rs` 把 CLI 用的全部核心能力暴露成库（`aox`）：

```
aox::agent      Agent 主循环（问模型 → 执行工具 → 回填结果 → 直到不再调用工具）
aox::llm        LlmProvider trait / ChatReply / StreamSink（流式思考）
aox::plugin     插件宿主（子进程 + stdio JSON-RPC）
aox::runtime    Runtime（工具注册表 / 授权策略 / 插件句柄）
aox::tool       元工具与工具抽象
aox::ui         Ui trait + AgentEvent  ← GUI 只需要实现这一个 trait
aox::paths      应用根解析 / 可写性 / 插件迁移
```

`src/main.rs`（CLI）不引用这个 lib，各模块由它自己的 crate root 再编译一份，
所以**CLI 的行为与加 GUI 之前逐字节等价**，不需要回归。

## 线程模型

```
┌────────────────────────┐   ToAgent   ┌──────────────────────────────┐
│ egui 主线程            │ ──────────▶ │ 后台线程：tokio 运行时       │
│  eframe::App (app.rs)  │             │  bridge::boot_and_run        │
│  每帧 pump() 收事件    │ ◀────────── │   aox::agent::Agent::run_turn│
└────────────────────────┘  FromAgent  └──────────────────────────────┘
```

- **两个方向都用 `tokio::sync::mpsc::unbounded_channel`**。`UnboundedSender` 是
  `Send + Sync + Clone`，正好满足 `StreamSink: Send + Sync` 的约束；主线程用
  `try_recv()` 非阻塞取事件（不需要运行时上下文）。
- **授权**用 `oneshot`：`GuiUi::authorize()` 把 `oneshot::Sender` 塞进 `AuthRequest`
  发给界面，然后 `await` 结果。界面上点按钮 / 按 `y|a|n` 就把它 `send` 回去。
- **中断**用 `Arc<SessionFlags>`（`AtomicBool`）+ 50ms 轮询，而不是 `Notify`。
  原因是 `Notify` 的许可会跨 turn 残留：空闲时按一次停止，会让**下一个** turn 立刻被中断。
  轮询在这个尺度上延迟无感，但语义是确定的。
- **退出**在 `Drop for App` 里做：`request_abort()` → 发 `Shutdown` → 有界等待 1.5s。
  `aox::plugin` 的子进程是 `kill_on_drop(true)` 的，所以正常收尾时插件会被清干净。

## 事件 → 界面

`AgentEvent`（与 CLI 同源）在 `app.rs::on_event` 里落成两类状态：

- **对话流** `items`：用户气泡 / 助手 Markdown / 折叠的过程块 / 提示。
  一轮结束时 `finish_turn()` 把进行中的 `Live` 收成 `Process`（默认折叠），
  再把 `pending_final` 作为最终答复追加到**后面**——顺序不能反。
- **活动流** `acts`：每一条事件都追加一行（带相对时间戳），右侧面板直接顺序渲染。
  流式思考是一条 `live` 行，增量原地追加、响应结束即冻结。

## 踩过的坑（改这个 crate 前先看）

1. **`Ui::horizontal_centered()` 会吃掉整个窗口。**
   它内部是 `allocate_ui_with_layout_dyn(available_size_before_wrap(), ..)` 且
   cross-align 为 `Center`，会把子 ui 撑到「全部可用高度」。放在**自动定高**的
   `TopBottomPanel` 里会形成正反馈：面板记录上一次的内容高度 → 内容又跟着面板变高 →
   面板继续变高，最终底栏把整个窗口吃光。用 `ui.horizontal()`（初始高度只有一行）代替。
2. **`TopBottomPanel` 不定高时会记住上一次的内容高度**（`PanelState`），
   所以任何会让内容高度依赖可用高度的写法都会失控。必要时显式给 `min_height` / `exact_height`。
3. **截图验证要 DPI aware。** `tools/gui_shot.ps1` 里第一件事是 `SetProcessDPIAware()`：
   否则非 DPI-aware 的 PowerShell 拿到的 `GetWindowRect` 是虚拟化坐标，而
   `CopyFromScreen` 抓的是物理像素，截图会整体偏移；只想要目标窗口还需要
   `SetWindowPos(HWND_TOPMOST)`，否则抓到的是盖在上面的别的程序。
4. **`egui::LayoutJob` 在 `egui::text::LayoutJob`**，不在 crate 根。
5. **字体**：egui 自带字体没有 CJK 字形（中文会变成空白方块）。`theme.rs` 从系统字体目录
   注入一个中文字体，并把它插在 emoji 字体**之前**（egui 内建的 `emoji-icon-font`
   会抢走一部分 CJK codepoint）。另注册一个字族做真粗体，否则 `**加粗**` 只能靠改颜色。
6. **版本**：`eframe 0.33` 是当前 rustc 1.90 能用的最高版本（0.34+ 要求 1.92/1.95）。

## 开发期验证

```bash
# 1) 造一个假的 OpenAI 兼容端点（可流式、带 reasoning_content、会调工具）
python tools/mock_llm.py 8791

# 2) 用测试配置启动，并自动发一条指令
target/debug/aox-gui.exe --config gui/test-config.toml --plugins gui/test-plugins \
    --prompt "现在有哪些插件和工具？"

# 3) 截图（Windows）
powershell -ExecutionPolicy Bypass -File tools/gui_shot.ps1 -Out gui/shot.png -W 0 -H 0
```

`gui/test-config.toml` 把 `meta_tools` 设为 `ask` 用于验证授权弹窗；
`gui/test-config-auto.toml` 设为 `auto`，用于一次性跑完整流程。

## 还没有做的

- 安装包 `installer/aox.iss` 目前只收 `aox.exe`。要一起分发 GUI，加一行
  `Source: "..\target\release\aox-gui.exe"`（并确保构建顺序先生成它）即可。
- 会话历史没有落盘，关掉窗口即散失（Agent 的 `messages` 在进程内）。
