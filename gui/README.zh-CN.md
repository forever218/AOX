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

## 视觉设计：纸与墨

配色只有**明度**，没有色相。两套调色板写在 `theme.rs` 里：

| | 纸（Light） | 墨（Dark） |
| --- | --- | --- |
| 底色 bg | `#F2EFE8` | `#141412` |
| 面板 panel | `#EBE7DE` | `#191917` |
| 卡片 surface | `#FAF8F3` | `#20201D` |
| 正文 text | `#1A1916` | `#EAE7DF` |

两条贯穿全局的约定：

- **语义状态不靠颜色**。成功/警告/失败的差异由**明度层次**（`ActKind::color()` 其实是一把
  「强调度」刻度）加上**反白块**表达：错误卡片整体反白（亮色=墨黑底白字，暗色=近白底黑字），
  时间轴节点用「实心点 / 空心环 / 打叉」区分。这样一套绘制代码同时服务两套主题，也天然色盲友好。
- **调色板必须运行时求值**。`P::bg()` 这类访问器每次都去读 `AtomicU8` 里的当前模式，
  所以切换主题立刻生效；如果写成 `const Color32` 就固化在编译期了，切换不会有效果。
  偏好落在 `<应用根>/.aox-gui-theme`（内容为 `light` / `dark`）。

切换按钮在顶栏最右：亮色模式显示月牙、暗色模式显示太阳。
`theme::sync_native_theme()` 会把 `ViewportCommand::SetTheme` 下发给窗口管理器，
让**原生标题栏**跟着一起变——否则纸白界面会顶着一条黑标题栏。

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
7. **「卡片被裁切」几乎都是宽度没锁死。** `ScrollArea` 的 `available_width` **包含滚动条占位**，
   子项按「文本理想宽度」撑开就会超出面板被裁。规矩：面板一进来就
   `let w = ui.available_width(); ui.set_width(w); ui.set_max_width(w);`，再把 `w` 传下去；
   frame 内部的子项要用**框架内部的** `ui.available_width()` 推算，而不是外层传进来的值
   （还要先减掉自身 `inner_margin`）。表格列宽别忘了扣列间距。
8. **窗口位置要显式指定，别交给操作系统。** Windows 会把新窗口「级联」到右下角，
   窗口下沿可能越过屏幕底边——用户看不到输入框，截图里也会出现一条**黑带**
   （`CopyFromScreen` 把屏幕外区域抓成了黑色，看着像渲染 bug，其实不是）。
   `app.rs::fit_window()` 里用 `ViewportCommand::OuterPosition` 居中摆放，并按
   `outer_rect - inner_rect` 实测的装饰厚度算外框尺寸。诊断时可以直接量：
   窗口 `Bottom` 与屏幕高度一比就知道。
9. **原生标题栏不归 egui 管。** 它是 DWM 画的，默认跟系统主题走。要同步得发
   `ViewportCommand::SetTheme`；注意创建窗口时下发的命令可能在前端事件循环起来之前就丢了，
   所以 `App` 里记一个 `native_theme: Option<Mode>`，首帧必发一次、之后只在变化时补发。
10. **「长文本不换行、被右边界裁掉」的真凶是 `Ui::wrap_mode()`。**
    它的判定顺序是：`style.wrap_mode` → `placer.grid()` → `layout()`。最后那一支写成
    `layout.is_vertical() || layout.is_horizontal() && layout.main_wrap()` 才返回 `Wrap`，
    而 `ui.horizontal()` / `horizontal_top()` 建的 `Layout::left_to_right` 的 `main_wrap` 是 **false**
    ⇒ 在水平布局里一律返回 **`Extend`**。
    `Label` 默认 `wrap_mode = None`，会去继承它，于是 `Label::layout_in_ui` 把
    `LayoutJob::wrap.max_width` 覆写成 `f32::INFINITY` —— 我们自己算好的宽度被原地丢掉，
    文本一行到底，冲出栏位右边界被 `ScrollArea` 裁切。气泡、列表项、工具调用行、表格单元格
    内部全是水平布局，所以到处都会中招。
    修法（两道）：
    - 在**栏根**（对话栏 / 活动流面板）打一把 `ui.style_mut().wrap_mode = Some(TextWrapMode::Wrap)`，
      后代 Ui（含 `Frame` 内部）会继承；
    - `markdown.rs` 里所有文本标签走 `wrap_label()`，逐个 `.wrap_mode(TextWrapMode::Wrap)` 兜底。
    注意 `break_anywhere` **不需要**打开：epaint 的断行候选是
    `word_boundary → dash → punctuation → any`，最后那个 `any` 已经能兜住超长无空格 token。
11. **量「有没有被裁」要用断言，不要用眼睛。** `markdown.rs` 里带了一个 headless 单测
    `text_never_exceeds_the_column`：用 `egui::Context::default()` + `RawInput.screen_rect`
    跑一遍真实的版心骨架（CentralPanel → ScrollArea → horizontal → add_space → vertical），
    断言 `ui.min_rect().width() <= inner`。改布局后 `cargo test -p aox-gui` 就能复现/防回归，
    比反复截图快一个数量级。
12. **「一用插件就弹黑色 python 窗口」是宿主拉起子进程的方式不对，不是插件的问题。**
    `aox-gui.exe` 用 `windows_subsystem = "windows"` 编译，自身**没有控制台**；此时用不带
    `CREATE_NO_WINDOW` 的方式拉一个控制台程序（`python.exe` / `node.exe` / `rustc.exe`），
    Windows 会**为它新分配一块带窗口的控制台**。修法就一行：
    `src/plugin/mod.rs::hide_console()` 给 `Command` 加 `creation_flags(0x0800_0000)`，
    **所有** spawn 路径都要经过它（`spawn_command` 覆盖首次启动 + `respawn` 自愈，`rustc` 编译单独一处）。
    实测（`tools/check_no_console_window.py --ab`，父进程 `DETACHED_PROCESS`）：

    | 子进程 creationflags | 子进程 `GetConsoleWindow()` | 新出现的控制台窗口 |
    |---|---|---|
    | `0`（对照组） | 非 0（如 2952730） | **1** |
    | `CREATE_NO_WINDOW` | `0` | **0** |

    两个容易走错的方向，别浪费时间去试：
    - **插件内部的 `subprocess` 不用管。** `CREATE_NO_WINDOW` 给插件进程的是一块
      「无窗口的控制台」，插件再 `shell=True` 拉 `cmd.exe`，孙子进程继承的是同一块无窗口控制台
      ⇒ 依然是 0 个窗口（`plugins/probe_runtime`、`sysinfo_pro` 就是这么写的，实测干净）。
      真正会漏的是**插件进程本身**被拉起的那一层。
    - **别去改插件 SDK 加防弹衣**，那是死代码 —— 除非哪天把宿主的启动标志从
      `CREATE_NO_WINDOW` 换成 `DETACHED_PROCESS`（那才是「彻底没控制台」，孙子进程会重新长出来）。
    - 注意 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`：
      **debug 版本身就是控制台程序**，`cargo run` 看到一个黑窗是正常的，别去修。

## 开发期验证

```bash
# 1) 造一个假的 OpenAI 兼容端点（可流式、带 reasoning_content、会调工具）
python tools/mock_llm.py 8791

# 2) 用测试配置启动，并自动发一条指令
target/debug/aox-gui.exe --config gui/test-config.toml --plugins gui/test-plugins \
    --prompt "现在有哪些插件和工具？"

# 3) 截图（Windows，二选一）
python tools/gui_shot.py --out gui/shot.png --w 0 --h 0        # 推荐：ctypes 调 user32，不依赖 PowerShell
powershell -ExecutionPolicy Bypass -File tools/gui_shot.ps1 -Out gui/shot.png -W 0 -H 0

# 4) 布局回归（不需要窗口）
cargo test -p aox-gui --bin aox-gui

# 5) 黑窗回归（插件子进程不许弹控制台窗口）
python tools/check_no_console_window.py --ab                          # 机制自检，秒级
python tools/check_no_console_window.py --app target/release/aox-gui.exe --cwd .
```

`tools/check_no_console_window.py` 的端到端模式用 `DETACHED_PROCESS` 启动 exe
（与「双击 / 点开始菜单」等价），一边跑一边枚举 `ConsoleWindowClass` 顶层窗口，
同时统计 `python.exe` 进程数以避免「插件压根没起来所以没窗口」的假通过。

`tools/gui_shot.py` 与 `.ps1` 等价（`MoveWindow` + `SetForegroundWindow` + `HWND_TOPMOST` + 抓屏 +
`SetProcessDPIAware`），但走 `ctypes` 直调 `user32`，不受 PowerShell 安全策略（`Add-Type` 被拦）
影响，而且失败时会打印窗口标题/DPI/逻辑尺寸，便于对齐坐标系。

`gui/test-config.toml` 把 `meta_tools` 设为 `ask` 用于验证授权弹窗；
`gui/test-config-auto.toml` 设为 `auto`，用于一次性跑完整流程。
只想看界面全貌、不连模型时用 `--demo`。

改界面时的几个效率要点：

- **`-W 0 -H 0`** 表示保持窗口原尺寸。传非 0 会走 `MoveWindow`，改 GL surface 尺寸容易出白条/花屏。
- **把 GUI 作为后台任务本身启动**（不要 `nohup … &` 再返回）——进程会随 bash 任务结束被回收，
  截图脚本就只能报 `NOTFOUND` / `BADRECT 0 x 0`。
- 重编前先 `taskkill` 掉旧实例，否则链接时 `LNK1104: 无法打开 aox_gui.exe`。
- **PowerShell 工具的 stdout 在某些环境下会被吞掉**（只回一句 `Command completed with exit code 0`）。
  脚本里改成 `Set-Content` 写文件，再用读文件的方式取结果。
- **`Add-Type`（P/Invoke）可能被安全层拦**。量窗口/屏幕几何改用 Python `ctypes` 直接调
  `user32`，比在 PowerShell 里绕更省事，也能顺手用 PIL 做像素探针交叉验证。
- 判断有没有裁切**不要只看截图缩略图**——缩放会造出「左边被裁」的假象，用像素探针查那一列即可。
- 想验证明暗切换是否真的生效，可以在确认窗口获得焦点后 `SetCursorPos` + `mouse_event` 真点一下，
  再看 `.aox-gui-theme` 有没有翻转。

## 还没有做的

- 安装包 `installer/aox.iss` 目前只收 `aox.exe`。要一起分发 GUI，加一行
  `Source: "..\target\release\aox-gui.exe"`（并确保构建顺序先生成它）即可。
- 会话历史没有落盘，关掉窗口即散失（Agent 的 `messages` 在进程内）。
