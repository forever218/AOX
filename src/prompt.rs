//! 系统提示词的「缝」。
//!
//! 提示词不再是一个编译期常量，而是若干**段（segment）**的有序拼接：
//!
//! | id | 来源 | 内容 |
//! |----|------|------|
//! | `core` | 内置 | 身份与核心事实 |
//! | `protocol` | 内置 | 插件协议、Python 模板、内置规则 |
//! | `env` | 运行时生成 | 宿主真实路径与本机解释器状况 |
//! | `rules` | 项目文件 | RULES.md 项目规则 |
//!
//! 用户可以在 `config.toml` 的 `[prompt]` 里重排（`order`）、禁用（`disable`）、
//! 追加（`[[prompt.segment]]`）任意段；同 id 的配置段会**覆盖**内置段 ——
//! 这与阶段 5「后层覆盖前层」是同一套语义，只是现在只有两层。
//!
//! # 逐字节不变的约定
//!
//! 段体**不带前后分隔符**，由 [`build`] 统一用 `"\n\n"` 连接。这样默认配置下拼出来的
//! 结果与历史上「常量 + `env_section` + 项目规则」的硬拼接**完全一致**。
//!
//! 这不是洁癖：服务端的 **prompt cache 命中率依赖前缀字节稳定**，前缀一变缓存全废，
//! 直接涨钱。本文件末尾的 `split_is_lossless` 与 `default_build_equals_legacy_assembly`
//! 两个测试钉死这一点。
//!
//! # 为什么内置常量是「一整块 + 运行时切分」
//!
//! 手工把那段文本拆成两段写进源码，等于把「抄错一个全角括号」的机会引进核心提示词。
//! 所以这里保留**原样的一整块** [`BUILTIN_PROMPT`]，在运行时按章节标题切分，
//! 并用测试证明「切完拼回去 == 原串」。

use std::path::PathBuf;

use crate::config::PromptConfig;
use crate::runtime::Runtime;

/// 内置提示词原文。
///
/// **这段文本一个字都不能改**（除非有意改提示词）—— 它与历史版本的 `SYSTEM_PROMPT`
/// 逐字节相同，由 `tools/` 之外的核对脚本 + 本文件测试共同保证。
const BUILTIN_PROMPT: &str = r#"你是 AOX，一个「自我增长」的 AI 代理，运行在用户本地的 Rust 宿主程序中。

# 核心事实
- 你初始没有任何实际工具，只有三个元工具：create_plugin、list_plugins、remove_plugin。
- 当任务需要某种能力（读写文件、执行命令、网络请求、数据处理……）时，你必须先用 create_plugin 自己编写一个插件来提供该能力，然后调用插件提供的工具完成任务。
- 插件持久保存在 plugins/ 目录，重启后自动加载。动手创建新插件前，先 list_plugins 检查是否已有可复用的能力。

# 插件协议
插件是独立进程，宿主通过 stdio 与之通信：每条消息是「一行 JSON」。stdout 只允许协议消息，日志一律写 stderr。
1. 启动后宿主发送: {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
   插件回复一行: {"jsonrpc":"2.0","id":1,"result":{"name":"<插件名>","tools":[{"name":"<工具名>","description":"<说明>","parameters":<JSON Schema>}]}}
2. 工具调用: {"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"<工具名>","arguments":{...}}}
   插件回复: {"jsonrpc":"2.0","id":N,"result":{"content":"<字符串结果>","is_error":false}}
3. 结束: 宿主发送 {"jsonrpc":"2.0","method":"shutdown"}（无 id），插件应退出。

# Python 插件模板（强烈推荐直接使用宿主提供的 SDK）
宿主已在 plugins/sdk/plugin_sdk.py 提供 SDK，插件源码这样写：

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from plugin_sdk import serve

def my_tool(args):
    # args 是 dict，返回任意可 str() 的结果
    return "结果字符串"

serve("<插件名>", {
    "my_tool": ("工具说明", {"type": "object", "properties": {"x": {"type": "string"}}}, my_tool),
})

# 规则
- 工具名全局唯一，必须用「插件名_功能」命名（例如 fs_read_file），避免与其他插件冲突。
- parameters 必须是合法的 JSON Schema object。
- 结果 content 必须是字符串；长结果尽量精炼。
- 每个插件保持单一职责；需要新能力就造新插件，也可以复用已有插件。
- 语言优先选 python；node 次之；rust 仅在没有依赖需求时使用（只能 std 单文件）。
- 插件禁止调用 input() 或任何等待人工输入的操作；除协议消息（单行 JSON）外不得向 stdout 写任何内容，调试信息一律写 stderr；每次写 stdout 后必须 flush（使用 SDK 则自动处理）。
- 插件 handler 必须快速返回；禁止死循环或长时间阻塞，否则会被判定超时并重启。
- 重建同名插件时直接用原名字即可（宿主的 create_plugin 会自动清理同名残留目录），不要为了绕开报错而改用 sysinfo2、sysinfo_pro 这类新名字。
- 插件工具默认自动执行；create_plugin / remove_plugin 等改变系统自身的元工具需要用户授权。
- 不要在只抛出一句「我先看看……」之类的计划后就结束回复。要么在同一条回复里直接发起工具调用，
  要么给出完整的最终结论；信息不足时明确说明还缺什么，而不是沉默结束。
- 任务完成后用自然语言向用户总结结果（不再调用任何工具即表示任务结束）。"#;

/// 在内置文本里切出 `protocol` 段的标记（含其上的空行）。
///
/// 原文里「# 核心事实」与「# 插件协议」之间是一个空行，所以标记是 `\n\n# 插件协议`。
const PROTOCOL_MARK: &str = "\n\n# 插件协议";

/// 标记里那段章节标题本身。
///
/// `split_once` 会把**整个标记**吃掉（包括标题），所以切完必须把标题补回去 ——
/// 否则提示词里会凭空少掉一行「# 插件协议」。这正是 `split_is_lossless` 测试
/// 第一次跑就抓到的 bug。
const PROTOCOL_HEAD: &str = "# 插件协议";

/// 内置段的 id。
pub const CORE: &str = "core";
pub const PROTOCOL: &str = "protocol";
/// 运行期生成的段 id。
pub const ENV: &str = "env";
/// 项目规则文件段 id。
pub const RULES: &str = "rules";

/// 配置追加段的默认排序键：排在内置段（10 / 20）与运行时段（30）之后。
const FALLBACK_ORDER: i32 = 50;

/// 段的来源。决定 `/prompt` 里的标注，也决定将来「哪一层覆盖哪一层」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 宿主内置
    Builtin,
    /// `config.toml` 的 `[[prompt.segment]]`
    Config,
    /// 宿主在运行期生成（运行环境）
    Runtime,
    /// `RULES.md` 等项目规则文件
    RulesFile,
    /// 组件贡献（阶段 6 · 6b 启用：插件经 `[contributes] prompt_segments` 声明，
    /// 正文由 `prompt/segment` 从子进程取回）
    Plugin,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Builtin => "内置",
            Source::Config => "配置",
            Source::Runtime => "运行时",
            Source::RulesFile => "项目文件",
            Source::Plugin => "插件",
        }
    }
}

/// 提示词的一段。
///
/// `body` **不带**前后分隔符 —— 连接由 [`build`] 统一负责。原因见模块文档。
#[derive(Debug, Clone)]
pub struct PromptSegment {
    /// 全局唯一。同名配置段会覆盖同 id 的内置段。
    pub id: String,
    /// 展示名（`/prompt` 用）。
    pub title: String,
    pub body: String,
    /// 默认排序键（升序）；同一 order 保持注册顺序（排序是稳定的）。
    pub order: i32,
    pub source: Source,
}

// ─────────────────────────── 内置段 ───────────────────────────

/// 内置的两段：`core` / `protocol`。
pub fn builtin_segments() -> Vec<PromptSegment> {
    let (core, protocol) = split_builtin();
    vec![
        PromptSegment {
            id: CORE.into(),
            title: "身份与核心事实".into(),
            body: core,
            order: 10,
            source: Source::Builtin,
        },
        PromptSegment {
            id: PROTOCOL.into(),
            title: "插件协议与内置规则".into(),
            body: protocol,
            order: 20,
            source: Source::Builtin,
        },
    ]
}

/// 把内置常量切成 `core` / `protocol` 两段。
///
/// 切不出来时退化成「整段算 core」而**不 panic** —— 提示词少一段，总比程序起不来强。
fn split_builtin() -> (String, String) {
    match BUILTIN_PROMPT.split_once(PROTOCOL_MARK) {
        Some((core, rest)) => (core.to_string(), format!("{PROTOCOL_HEAD}{rest}")),
        None => (BUILTIN_PROMPT.to_string(), String::new()),
    }
}

// ─────────────────────────── 运行期生成的段 ───────────────────────────

/// 运行环境段。
///
/// **原样搬自 `src/agent.rs`**（逐字节保留，含开头的 `\n\n`，由 [`strip_sep`] 剥掉）。
/// 它把宿主的真实路径、以及「本机到底有没有 python / node / rustc」如实告诉模型 ——
/// 否则模型会在裸机上反复创建注定失败的 python 插件，把整轮任务耗光。
fn env_section(rt: &Runtime) -> String {
    let mut s = format!(
        "\n\n# 运行环境（由宿主确定，直接使用，勿猜测）\n\
         - 应用根目录（＝插件进程的工作目录）: {root}\n\
         - 插件目录: {plugins}\n\
         - 插件源码路径形如: {plugins}\\<插件名>\\main.py\n\
         - 平台: {os}\n\
         - 所有相对路径都相对「应用根目录」。涉及本项目文件时，直接用上面这些绝对路径最稳妥。\n\
         - 插件子进程由宿主以「应用根目录」为工作目录启动，因此插件内的相对路径与你在宿主机上看到的一致。\n\
         - Python 插件已由宿主强制 UTF-8（PYTHONUTF8=1，PYTHONIOENCODING=utf-8），你无需再写编码兼容代码。\n\
         - 你的插件目录里只会有一个 plugins/（就是上面这个），不要再去 target/ 等构建产物目录里找。",
        root = rt.root.display(),
        plugins = rt.plugins_dir.display(),
        os = std::env::consts::OS,
    );
    // 把「本机到底有没有 python / node / rustc」如实告诉模型。
    // 否则它会在裸机上反复创建注定失败的 python 插件，把整轮任务耗光。
    s.push_str(&crate::runtimes::prompt_section(&rt.root, &rt.config.runtimes));
    s
}

fn env_segment(rt: &Runtime) -> PromptSegment {
    PromptSegment {
        id: ENV.into(),
        title: "运行环境".into(),
        body: strip_sep(env_section(rt)),
        order: 30,
        source: Source::Runtime,
    }
}

/// 项目规则段（`RULES.md`）。文件不存在或内容为空时返回 `None`。
fn rules_segment(rt: &Runtime) -> Option<PromptSegment> {
    let path = rt.config.agent.rules_file.as_deref()?;
    let path = PathBuf::from(path);
    let path = if path.is_absolute() { path } else { rt.root.join(path) };
    let text = std::fs::read_to_string(path).ok()?;
    let body = text.trim();
    if body.is_empty() {
        return None;
    }
    Some(PromptSegment {
        id: RULES.into(),
        title: "项目规则（RULES.md）".into(),
        // 历史格式是 "\n\n# 项目规则\n" + 正文；前缀那部分由 build() 补 "\n\n"，
        // 所以这里只需要 "# 项目规则\n" + 正文。
        body: format!("# 项目规则\n{body}"),
        order: 40,
        source: Source::RulesFile,
    })
}

/// 剥掉段首用于拼接的 `\n\n`。段体由 [`build`] 统一补回，不剥就会多出两个空行。
fn strip_sep(s: String) -> String {
    match s.strip_prefix("\n\n") {
        Some(rest) => rest.to_string(),
        None => s,
    }
}

// ─────────────────────────── 注册表 ───────────────────────────

/// 供 `Runtime::new` 构造初始注册表：内置段 + 配置段（同 id 覆盖内置）。
///
/// 「配置覆盖内置」是刻意的 —— 它让 `[[prompt.segment]]` 既能追加新段，
/// 也能整段替换掉内置的 `core` / `protocol`，而不必先去 `disable` 再补一段。
pub fn registry_from_config(cfg: &PromptConfig) -> Vec<PromptSegment> {
    let mut segs = builtin_segments();
    for extra in &cfg.segment {
        let seg = PromptSegment {
            id: extra.id.trim().to_string(),
            title: extra.title.clone().unwrap_or_else(|| extra.id.trim().to_string()),
            body: extra.body.trim().to_string(),
            order: extra.order.unwrap_or(FALLBACK_ORDER),
            source: Source::Config,
        };
        if seg.id.is_empty() {
            continue;
        }
        match segs.iter_mut().find(|s| s.id == seg.id) {
            Some(slot) => *slot = seg,
            None => segs.push(seg),
        }
    }
    segs
}

// ─────────────────────────── 组装 ───────────────────────────

/// 当前生效的全部段（未排序、未过滤）：注册表里的 + 运行期生成的。
pub fn collect(rt: &Runtime) -> Vec<PromptSegment> {
    let mut segs = rt.prompt_segments();
    segs.push(env_segment(rt));
    if let Some(r) = rules_segment(rt) {
        segs.push(r);
    }
    segs
}

/// 按配置过滤 + 排序后的最终序列 —— 这是 `/prompt` 展示、也是 [`build`] 使用的那一份。
pub fn assemble(rt: &Runtime) -> Vec<PromptSegment> {
    let cfg = &rt.config.prompt;
    let mut segs = collect(rt);

    // 1. 禁用
    segs.retain(|s| !cfg.disable.iter().any(|d| d.trim() == s.id));
    // 2. 丢掉空段（配置段写空了就没有必要占一个 "\n\n"）
    segs.retain(|s| !s.body.is_empty());
    // 3. 默认序（稳定排序，同 order 保持注册顺序）
    segs.sort_by_key(|s| s.order);

    // 4. 显式 order：列出的 id 按列出顺序排到最前，未列出的保持默认序跟在其后。
    //    这样 `order = ["core", "env"]` 就是「把 env 挪到 core 后面」，
    //    而不需要把四段全列一遍。
    if !cfg.order.is_empty() {
        let mut head: Vec<PromptSegment> = Vec::with_capacity(segs.len());
        for id in &cfg.order {
            let id = id.trim();
            if let Some(pos) = segs.iter().position(|s| s.id == id) {
                head.push(segs.remove(pos));
            }
        }
        head.extend(segs);
        segs = head;
    }
    segs
}

/// 拼出最终的系统提示词。
pub fn build(rt: &Runtime) -> String {
    assemble(rt)
        .iter()
        .map(|s| s.body.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `/prompt` 命令的正文：段清单 + 每段的来源与首行。
///
/// 刻意**不做列对齐**：段 id 是 ASCII 而标题是中文，用 `{:<10}` 之类补齐只会被
/// 汉字的双倍宽度顶歪。改成「一行一行列出来」反而更好读。
pub fn report(rt: &Runtime) -> String {
    let segs = assemble(rt);
    let total: usize = segs.iter().map(|s| s.body.chars().count()).sum();
    let mut out = format!(
        "系统提示拼装结果 · {} 段 · {} 字符\n",
        segs.len(),
        crate::llm::thousands(total as u64)
    );
    for (i, s) in segs.iter().enumerate() {
        let first = s.body.lines().next().unwrap_or("").trim();
        out.push_str(&format!(
            "\n  {}. {}（{}）· {}\n     {}\n",
            i + 1,
            s.id,
            s.source.label(),
            s.title,
            elide(first, 60)
        ));
    }
    let (all, shown) = (collect(rt).len(), segs.len());
    if all > shown {
        out.push_str(&format!("\n  （另有 {} 段被 disable 或内容为空，未参与拼装）", all - shown));
    }
    out.push_str("\n  改 config.toml 的 [prompt] 可重排 / 禁用 / 追加段。");
    out
}

fn elide(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, PromptConfig, PromptSegmentConfig, RuntimesConfig};

    fn rt_with(prompt: PromptConfig) -> std::sync::Arc<Runtime> {
        let mut cfg = Config::dummy();
        // 让 env 段的内容稳定：不走三档解析，直接给一个不存在的命令名。
        cfg.runtimes = RuntimesConfig {
            python: "__none__.exe".into(),
            node: "__none__.exe".into(),
            rustc: "__none__.exe".into(),
            python_url: None,
            python_sha256: None,
        };
        cfg.prompt = prompt;
        let root = std::env::temp_dir().join("aox-prompt-test");
        Runtime::new(cfg, root.clone(), root.join("plugins")).expect("测试用运行时不应因存储后端失败")
    }

    /// 切分必须无损：切完原样拼回去要等于原串。
    #[test]
    fn split_is_lossless() {
        let (core, protocol) = split_builtin();
        assert!(!core.is_empty() && !protocol.is_empty(), "两段都不应为空");
        assert_eq!(format!("{core}\n\n{protocol}"), BUILTIN_PROMPT);
    }

    /// 默认配置下，拼装结果必须与历史实现**逐字节相同**：
    /// `SYSTEM_PROMPT + env_section(rt)`（test 环境没有 RULES.md，故不含项目规则段）。
    #[test]
    fn default_build_equals_legacy_assembly() {
        let rt = rt_with(PromptConfig::default());
        let legacy = format!("{BUILTIN_PROMPT}{}", env_section(&rt));
        assert_eq!(build(&rt), legacy);
    }

    /// 段体一律不带前后分隔符 —— 带了就会在拼接处出现四个换行。
    #[test]
    fn segment_bodies_carry_no_separators() {
        let rt = rt_with(PromptConfig::default());
        for s in assemble(&rt) {
            assert!(!s.body.starts_with('\n'), "段 {} 以换行开头", s.id);
            assert!(!s.body.ends_with('\n'), "段 {} 以换行结尾", s.id);
        }
    }

    #[test]
    fn disable_removes_segment() {
        let rt = rt_with(PromptConfig { disable: vec![ENV.into()], ..Default::default() });
        let ids: Vec<_> = assemble(&rt).iter().map(|s| s.id.clone()).collect();
        assert!(!ids.contains(&ENV.to_string()));
        assert!(ids.contains(&CORE.to_string()));
    }

    #[test]
    fn order_moves_segment_to_front() {
        let rt = rt_with(PromptConfig { order: vec![ENV.into()], ..Default::default() });
        let ids: Vec<_> = assemble(&rt).iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids.first().map(String::as_str), Some(ENV));
        // 未列出的段保持默认序跟在后面
        assert_eq!(&ids[1..], &[CORE.to_string(), PROTOCOL.to_string()]);
    }

    #[test]
    fn config_segment_is_appended() {
        let rt = rt_with(PromptConfig {
            segment: vec![PromptSegmentConfig {
                id: "style".into(),
                title: Some("团队风格".into()),
                body: "先给结论再给理由。".into(),
                order: None,
            }],
            ..Default::default()
        });
        let segs = assemble(&rt);
        let last = segs.last().unwrap();
        assert_eq!(last.id, "style");
        assert_eq!(last.source, Source::Config);
        assert!(build(&rt).ends_with("先给结论再给理由。"));
    }

    /// 同 id 的配置段整段替换内置段（而不是并存两份）。
    #[test]
    fn config_segment_overrides_builtin() {
        let rt = rt_with(PromptConfig {
            segment: vec![PromptSegmentConfig {
                id: CORE.into(),
                title: None,
                body: "你是一个只做代码评审的助手。".into(),
                order: None,
            }],
            ..Default::default()
        });
        let segs = assemble(&rt);
        let core = segs.iter().find(|s| s.id == CORE).unwrap();
        assert_eq!(core.source, Source::Config);
        assert_eq!(core.body, "你是一个只做代码评审的助手。");
        // 内置正文不许再出现
        assert!(!build(&rt).contains("你初始没有任何实际工具"));
    }

    /// 被禁用的内置段不得再出现在最终提示词里。
    #[test]
    fn disabled_builtin_text_is_gone() {
        let rt = rt_with(PromptConfig { disable: vec![PROTOCOL.into()], ..Default::default() });
        let out = build(&rt);
        assert!(!out.contains("# 插件协议"));
        assert!(out.contains("# 核心事实"));
    }
}
