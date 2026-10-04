mod agent;
mod agent_loop;
mod auth;
mod config;
mod history;
mod hook;
mod llm;
mod message;
mod plugin;
mod prompt;
mod runtime;
mod runtimes;
mod sandbox;
mod selftest;
mod store;
mod tool;
mod ui;
mod workplace;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::agent::Agent;
use crate::llm::openai::OpenAiProvider;
use crate::llm::LlmProvider;
use crate::runtime::Runtime;
use crate::tool::{hostfs, meta};
use crate::ui::cli::CliUi;
use crate::ui::{AgentEvent, Ui};

/// 双击 exe 运行时（无终端），出错后等待用户按键再退出，避免窗口一闪而过。
fn pause_before_exit() {
    use std::io::Write as _;
    print!("\n按回车键退出…");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}

/// 解析「应用根目录」。
///
/// 规则：从 exe 所在目录向上找最近的 Cargo.toml；找到就用它所在目录（开发态），
/// 否则就用 exe 所在目录（分发态）。这样 `cargo run`（exe 在 target/debug）与双击
/// exe 都会归一到同一个根，不会再出现 target/debug/plugins/ 与源码 plugins/ 两套目录
/// 并存、模型在文件系统里看到两个 plugins 的路径混乱。
fn resolve_app_root(exe_dir: Option<&Path>) -> PathBuf {
    let Some(exe_dir) = exe_dir else {
        return PathBuf::from(".");
    };
    let mut cur: Option<&Path> = Some(exe_dir);
    while let Some(d) = cur {
        if d.join("Cargo.toml").exists() {
            return d.to_path_buf();
        }
        cur = d.parent();
    }
    exe_dir.to_path_buf()
}

/// 把旧位置（exe 目录下）的插件迁到规范插件目录，避免历史插件「凭空消失」。
/// 只补不存在的，不覆盖已有文件；全部迁移成功后，清理掉遗留的构建产物目录
/// （含 target 路径分量），否则下次启动会把用户已删除的插件重新「复活」。
fn migrate_plugins(from: &Path, to: &Path) {
    if from == to || !from.is_dir() {
        return;
    }
    let mut failed = 0usize;
    if let Ok(entries) = std::fs::read_dir(from) {
        for e in entries.flatten() {
            let src = e.path();
            if !src.is_dir() {
                continue;
            }
            if e.file_name() == "sdk" {
                continue; // sdk 由宿主每次启动重写，不迁移
            }
            // 只迁移结构完整的插件（含 plugin.toml），跳过上次失败留下的空目录
            if !src.join("plugin.toml").is_file() {
                continue;
            }
            let dst = to.join(e.file_name());
            if dst.exists() {
                continue;
            }
            if copy_dir(&src, &dst).is_err() {
                failed += 1;
            }
        }
    }
    let _ = std::fs::create_dir_all(to);
    if failed == 0 && to.is_dir() && from.components().any(|c| c.as_os_str() == "target") {
        let _ = std::fs::remove_dir_all(from);
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&s, &d)?;
        } else if ty.is_file() {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

/// 目录是否可写（用一个探测文件判断）。
fn is_writable_dir(dir: &Path) -> bool {
    let probe = dir.join(".aox-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 保证应用根可写。AOX 运行时要在根目录下写 config.toml 与 plugins/，
/// 若被装到 Program Files 这类只读位置，就退回到用户可写的 %LOCALAPPDATA%\AOX。
fn ensure_writable_root(root: PathBuf) -> PathBuf {
    if is_writable_dir(&root) {
        return root;
    }
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return root;
    };
    let alt = PathBuf::from(local).join("AOX");
    if std::fs::create_dir_all(&alt).is_ok() && is_writable_dir(&alt) {
        println!("提示: {} 不可写，改用 {}", root.display(), alt.display());
        alt
    } else {
        root
    }
}

/// `/runtime` 子命令：查看状态 / 装配运行时。
///
/// 解析与装配都在 `crate::runtimes` 里，CLI 与 GUI 共用同一份逻辑与文案，
/// 这里只负责决定「进度和结果打印到哪」。
async fn runtimes_command(rt: &Arc<Runtime>, line: &str) -> Result<String> {
    match runtimes::parse_cmd(line)? {
        runtimes::Cmd::Status => Ok(runtimes::status_report(&rt.root, &rt.config.runtimes)),
        runtimes::Cmd::Dir => Ok(rt.root.join("runtime").display().to_string()),
        runtimes::Cmd::Install(lang) => {
            // 进度直接打屏（装配可能要下载 10 MB，不能一声不吭）
            let mut sink = |m: &str| println!("  {}", ui::style::dim(m));
            runtimes::install(rt, lang, &mut sink).await
        }
    }
}

/// `/history` 子命令：查看 / 切换 / 删除历史对话。
///
/// 解析与文案都在 `crate::history` 里，CLI 与 GUI 共用同一份实现。
/// 数据一律向**存储缝**（`crate::store::Store`）要 —— 这里不再拼任何文件路径，
/// 换后端（比如一个库装下所有对话）时这段代码一行都不用改。
/// 只有 `open` 需要拿到 Agent —— 它要就地换掉上下文，这是唯一一个
/// 「不是纯查询」的子命令。
fn history_command(agent: &mut Agent, line: &str, store: &dyn store::Store) -> Result<String> {
    Ok(match history::parse_cmd(line)? {
        history::Cmd::List => history::list_text(
            &store.list(history::DEFAULT_LIST_LIMIT),
            store.location(),
            agent.history_id(),
        ),
        // 把「用的是哪个后端」一并说出来：以后真加了 sqlite，用户看到这行就知道
        // 自己改的配置生效了没有，而不用去猜。
        history::Cmd::Dir => format!("{}（后端 {}）", store.location().display(), store.backend()),
        history::Cmd::Show(key) => {
            let s = store.resolve(&key)?;
            let items = store.load(&s.id)?;
            format!(
                "{}  {}（{} 条）\n{}",
                s.time,
                s.title,
                s.messages,
                history::transcript_text(&items)
            )
        }
        history::Cmd::Open(key) => {
            let s = store.resolve(&key)?;
            let n = agent.resume(&s.id)?;
            format!(
                "已切换到「{}」（{} 条消息）。接下来的发言会继续追加到同一个文件。\n  {}",
                s.title,
                n,
                s.path.display()
            )
        }
        history::Cmd::Remove(key) => {
            let s = store.remove(&key)?;
            format!("已删除: {}  {}", s.time, s.title)
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    ui::style::init(); // 探测终端能力：启用 ANSI 配色 + UTF-8

    // 应用根目录：cargo run 时定位到 Cargo.toml 所在目录，双击 exe 时用 exe 目录。
    // 插件目录固定为 <root>/plugins，可用 --plugins <path> 覆盖。
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let app_root = ensure_writable_root(resolve_app_root(exe_dir.as_deref()));
    let plugins_dir = args
        .windows(2)
        .find(|w| w[0] == "--plugins")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| app_root.join("plugins"));
    if let Some(d) = exe_dir.as_ref() {
        migrate_plugins(&d.join("plugins"), &plugins_dir);
    }
    plugin::ensure_sdk(&plugins_dir)?;

    // ---- selftest 子命令：不依赖 LLM 验证插件链路
    if args.iter().any(|a| a == "selftest") {
        return selftest::run(app_root, plugins_dir).await;
    }

    // ---- 加载配置（锚定应用根，避免因 CWD 不同而读写到意外位置；
    //      此前用 CWD 相对路径，双击安装版时会把 config.toml 写到 C:\ 导致「拒绝访问」闪退）
    let config_path = args
        .windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| app_root.join("config.toml"));

    // 组合清单（阶段 5）：配置由 `profile → bundle → patch → config.toml` 合成，
    // **config.toml 永远是最后一层**，所以以前写的配置一个字都不用改。
    // 应用根下没有 aox.toml 时，层栈退化成「只有 config.toml 一条」—— 与以前完全等价。
    let manifest_arg = args
        .windows(2)
        .find(|w| w[0] == "--manifest")
        .map(|w| PathBuf::from(&w[1]));
    let profile_arg = args.windows(2).find(|w| w[0] == "--profile").map(|w| w[1].clone());

    // `--dump-config`：只把层栈与合成后的树打出来，然后退出。
    // 特意走 `compose`（不解析 Config）—— 配置坏了的时候，恰恰最需要看这张表。
    if args.iter().any(|a| a == "--dump-config") {
        match config::layer::compose(
            &app_root,
            &config_path,
            manifest_arg.as_deref(),
            profile_arg.as_deref(),
        ) {
            Ok(c) => println!("{}", c.dump_text()),
            Err(e) => println!("[配置错误] {e:#}"),
        }
        return Ok(());
    }

    let resolved = match config::layer::resolve(
        &app_root,
        &config_path,
        manifest_arg.as_deref(),
        profile_arg.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            if !config_path.exists() {
                // 首次运行：自动生成模板，避免双击 exe 时窗口一闪而过
                match std::fs::write(&config_path, include_str!("../config.example.toml")) {
                    Ok(()) => {
                        println!("未找到配置文件，已为你创建模板: {}", config_path.display());
                        println!("请用编辑器打开它，填写 base_url 和 api_key（或 api_key_env），然后重新启动本程序。");
                    }
                    Err(werr) => {
                        println!("无法创建配置文件 {}: {werr}", config_path.display());
                        println!("请确认该目录可写，或用 --config <路径> 指定一个可写位置。");
                    }
                }
            } else {
                println!("[配置错误] {e:#}");
            }
            pause_before_exit();
            return Ok(());
        }
    };
    // `/config` 要给人看「这份配置是哪几层合出来的」，所以先把短版面留下来 ——
    // 配置本体随后就被 Runtime 拿走了。
    let layer_summary = resolved.composed.layer_summary();
    let config = resolved.config;

    // ---- 组装运行时：元工具 + 恢复持久化插件
    // `[store] backend` 写错会在这里失败。按「配置错误」对待：友好提示 + 等按键，
    // 与上面 TOML 解析失败同一条路径 —— 双击 exe 的用户不会只看到窗口一闪。
    let rt = match Runtime::new(config, app_root.clone(), plugins_dir) {
        Ok(rt) => rt,
        Err(e) => {
            println!("[配置错误] {e:#}");
            pause_before_exit();
            return Ok(());
        }
    };
    for t in meta::meta_tools() {
        rt.register(t).await?;
    }
    // 宿主文件工具（沙箱方案 ③：把文件能力收回宿主）。
    // 参数与实现都在宿主手里，所以「路径必须先过工作区判定」无法被绕过 ——
    // 与 `meta_tools` 同一批注册；默认档位（full）下它们与普通读写工具无异。
    for t in hostfs::host_file_tools() {
        rt.register(t).await?;
    }

    let mut ui = CliUi::new();
    // 恢复持久化插件。成功的静默（启动横幅保持干净），失败才出声。
    // 想查看已加载的插件请用 /plugins。
    let load_results = plugin::load_all(&rt).await;
    for (name, result) in &load_results {
        if let Err(e) = result {
            ui.emit(AgentEvent::Warn(format!("插件 {name} 加载失败: {e:#}")));
        }
    }
    // 围栏没套上的插件要出声 —— 这是「以为有保护、其实没有」的典型场景，
    // 与「插件加载失败」同属「用户必须知道、但不该中断启动」的那一类。
    // 判定与文案由 `sandbox::unfenced` 统一提供（GUI 那边共用同一份）。
    let unfenced = sandbox::unfenced(&rt).await;
    if !unfenced.is_empty() {
        ui.emit(AgentEvent::Warn(format!(
            "以下插件没套上进程围栏，它们不受进程数 / 内存 / UI 限制：{}",
            unfenced.join("；")
        )));
    }

    let llm: Arc<dyn LlmProvider> = match OpenAiProvider::new(&rt.config.provider) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            ui.emit(AgentEvent::Error(format!("LLM 未就绪: {e:#}")));
            ui.emit(AgentEvent::Info(
                "可在 config.toml 填写 base_url / api_key（或设置 api_key_env 指向的环境变量）；\
                 本地命令 /tools /plugins /demo 仍可用。"
                    .into(),
            ));
            Arc::new(llm::UnavailableProvider { reason: format!("{e:#}") })
        }
    };

    // 项目规则文件（RULES.md）不再在这里读：它现在是提示词的 `rules` 段，
    // 由 `prompt::build` 统一从 `config.agent.rules_file` 取 —— CLI 与 GUI 共用一份实现。
    let mut agent = Agent::new(llm, rt.clone());

    println!("\n{}", ui::style::logo());
    println!(
        "  {} {}",
        ui::style::dim("模型"),
        ui::style::bold(&rt.config.provider.model)
    );
    ui.emit(AgentEvent::Info(format!(
        "配置 {}   插件目录 {}",
        config_path.display(),
        rt.plugins_dir.display()
    )));
    ui.emit(AgentEvent::Info("输入 /help 查看所有命令".into()));
    ui.emit(AgentEvent::Info(format!(
        "项目主页 {}",
        // OSC 8 超链接：Windows Terminal / VS Code 等可直接点击跳转；
        // 老 conhost 不支持时会自动退回纯文本 URL
        ui::style::link(ui::style::PROJECT_URL, ui::style::PROJECT_URL)
    )));

    // 运行时提示：只在 Python 缺失时多出一行，运行时齐备时启动输出保持干净。
    if let Some(hint) = runtimes::missing_hint(&rt.root, &rt.config.runtimes) {
        ui.emit(AgentEvent::Warn(hint));
    }

    // ---- REPL
    loop {
        let Some(line) = ui.read_user_input().await? else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line {
            "/quit" | "/exit" => break,
            "/tools" => {
                ui.emit(AgentEvent::Info(format!("工具: {}", rt.tool_names().await.join(", "))));
            }
            "/plugins" => {
                let plugins = rt.plugins.lock().await;
                if plugins.is_empty() {
                    ui.emit(AgentEvent::Info("（无已加载插件）".into()));
                } else {
                    let mut names: Vec<_> = plugins.keys().cloned().collect();
                    names.sort();
                    // 顺带把**围栏状态**带出来：`围栏 ✗` 表示这个进程没进 Job
                    // （沙箱关着、或外层 Job 不允许嵌套），它不是致命错误，
                    // 但「以为有保护、其实没有」值得被看一眼。
                    let listed: Vec<String> = names
                        .iter()
                        .map(|n| match plugins.get(n) {
                            Some(h) => format!("{n}（{}）", h.fence_status()),
                            None => n.clone(),
                        })
                        .collect();
                    ui.emit(AgentEvent::Info(format!("插件: {}", listed.join(", "))));
                    // 组件贡献（阶段 6 · 6b）：只列**有贡献**的那些 —— 只提供工具的
                    // 老插件不在这儿重复出现（上面那行已经说过它们了）。
                    let mut extra: Vec<String> = Vec::new();
                    for n in &names {
                        let Some(h) = plugins.get(n) else { continue };
                        let c = &h.contributes;
                        if c.is_empty() {
                            continue;
                        }
                        let mut parts: Vec<String> = Vec::new();
                        if !c.hooks.is_empty() {
                            parts.push(format!("钩子 {}", c.hooks.join("/")));
                        }
                        if !c.prompt_segments.is_empty() {
                            parts.push(format!("提示词段 {}", c.prompt_segments.join("/")));
                        }
                        if !c.caps.is_empty() {
                            parts.push(format!("申请能力 {}", c.caps.join("/")));
                        }
                        if c.theme {
                            // 顺带读一下补丁本身：声明了却为 None，说明取回或校验没过
                            parts.push(if h.theme_patch.is_some() {
                                "主题补丁 ✓（GUI 合并待接线）".to_string()
                            } else {
                                "主题补丁（没取到）".to_string()
                            });
                        }
                        extra.push(format!("  {n} → {}", parts.join("；")));
                    }
                    if !extra.is_empty() {
                        ui.emit(AgentEvent::Info(format!("组件贡献：\n{}", extra.join("\n"))));
                    }
                }
            }
            "/demo" => {
                crate::ui::cli::print_demo();
                // 顺带演示：等待动画 → 工具过程被折叠 → /expand 展开
                ui.turn_start().await;
                ui.set_status("思考中").await;
                tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                ui.emit(AgentEvent::ToolCall {
                    name: "fs_read".into(),
                    args: serde_json::json!({"path": "config.toml"}),
                });
                ui.emit(AgentEvent::ToolRunning { name: "fs_read".into(), timeout_secs: 120 });
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                ui.emit(AgentEvent::ToolResult {
                    name: "fs_read".into(),
                    is_error: false,
                    preview: "base_url = \"https://api.deepseek.com/v1\"".into(),
                });
                ui.emit(AgentEvent::AssistantText(
                    "演示结束：上面的工具调用过程已折叠，输入 **/expand** 可展开查看。".into(),
                ));
                ui.turn_end().await;
            }
            "/expand" => ui.expand_last_turn(),
            "/usage" => ui.show_usage(agent.usage_log()),
            "/prompt" => ui.emit(AgentEvent::Info(prompt::report(&rt))),
            "/hook" => ui.emit(AgentEvent::Info(hook::report(&rt))),
            "/loop" => ui.emit(AgentEvent::Info(agent_loop::report(&rt))),
            "/sandbox" => ui.emit(AgentEvent::Info(sandbox::report(&rt))),
            // 工作区：`/workplace` 看现状，`/workplace full|default|<目录>` 切档位。
            // 判定与执行都在 `workplace` 模块里（一份实现，CLI 与 GUI 共用）。
            _ if workplace::is_cmd(line) => match workplace::command(&rt, line) {
                Ok(text) => {
                    ui.emit(AgentEvent::Info(text));
                    // 档位要跟着记进本对话的存档。「看现状」那条也会走到这里，
                    // 但 `persist_workplace` 自己比对过、没变就不写。
                    agent.persist_workplace();
                }
                Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
            },
            "/config" => ui.emit(AgentEvent::Info(layer_summary.clone())),
            "/new" => {
                // 与 GUI 顶栏那枚「+」同一条路径。旧对话不需要保存动作 ——
                // 它一直是边聊边追加的，此刻已经在 history/ 里躺好了。
                agent.new_conversation();
                ui.emit(AgentEvent::Info(
                    "已开一段新对话。上一段已经保存在 history/ 里（/history 可查）。".into(),
                ));
            }
            _ if runtimes::is_cmd(line) => {
                match runtimes_command(&rt, line).await {
                    Ok(text) => ui.emit(AgentEvent::Info(text)),
                    Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
                }
            }
            _ if history::is_cmd(line) => {
                match history_command(&mut agent, line, &*rt.store) {
                    Ok(text) => ui.emit(AgentEvent::Info(text)),
                    Err(e) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
                }
            }
            "/help" => {
                println!();
                for (cmd, desc) in [
                    ("/help", "显示本帮助"),
                    ("/tools", "列出当前所有可用工具"),
                    ("/plugins", "列出已加载的插件，以及各自贡献了什么（钩子 / 提示词段 / 能力）"),
                    ("/runtime", "查看插件运行时状态；/runtime install python 装配内置运行时"),
                    (
                        "/history",
                        "历史对话；list 列表，show <序号> 看全文，open <序号> 接着聊，rm <序号> 删除",
                    ),
                    ("/new", "开一段新对话（当前这段已自动保存进历史）"),
                    ("/demo", "预览终端排版与过程折叠效果"),
                    ("/expand", "展开上一次被折叠的工具调用过程"),
                    ("/usage", "回看上一次任务的 token 用量明细"),
                    ("/prompt", "查看系统提示词的分段拼装结果（config.toml 的 [prompt] 可重排/禁用/追加）"),
                    ("/hook", "查看生效的循环钩子（来自 config.toml 的 [hook] 与插件的 [contributes]）"),
                    (
                        "/loop",
                        "查看主循环实现与上下文裁剪（config.toml 的 [loop]：impl 换实现、max_rounds 限轮数）",
                    ),
                    (
                        "/sandbox",
                        "查看插件子进程的围栏（config.toml 的 [sandbox]：进程数 / 内存 / UI 限制；也写明它管不了什么）",
                    ),
                    (
                        "/workplace",
                        "查看/切换模型的文件边界；/workplace full|default|<目录> 切档位（config.toml 的 [workplace]）",
                    ),
                    (
                        "/config",
                        "查看这份配置由哪几层合成（应用根下的 aox.toml 可自定义；--dump-config 看每个值的来源）",
                    ),
                    ("/quit", "退出程序（/exit 等价）"),
                ] {
                    println!(
                        "  {}  {}",
                        ui::style::cyan(&format!("{cmd:<10}")),
                        ui::style::dim(desc)
                    );
                }
                println!(
                    "  {}",
                    ui::style::dim("其余输入直接发给模型；任务进行中按 Ctrl+C 可中断")
                );
                println!();
            }
            _ => {
                let mark = agent.messages_len();
                enum TurnOutcome {
                    Done(Result<()>),
                    Interrupted,
                }
                // select! 分支内不能再访问被 turn 借用的 agent/ui，先收集结果再处理
                let outcome = {
                    let turn = agent.run_turn(&mut ui, line);
                    tokio::pin!(turn);
                    tokio::select! {
                        r = &mut turn => TurnOutcome::Done(r),
                        _ = tokio::signal::ctrl_c() => TurnOutcome::Interrupted,
                    }
                };
                match outcome {
                    TurnOutcome::Done(Ok(())) => {}
                    TurnOutcome::Done(Err(e)) => ui.emit(AgentEvent::Error(format!("{e:#}"))),
                    TurnOutcome::Interrupted => {
                        ui.abort_turn().await;
                        agent.truncate_messages(mark);
                        ui.emit(AgentEvent::Warn("已中断当前任务（对话已回滚到任务前状态）".into()));
                    }
                }
            }
        }
    }

    // ---- 退出前关闭所有插件进程
    let handles: Vec<_> = rt.plugins.lock().await.values().cloned().collect();
    for h in handles {
        h.shutdown().await;
    }
    Ok(())
}
