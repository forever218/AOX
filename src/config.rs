use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// 分层组合清单（阶段 5）：把「读一份配置」变成「合成一个层栈」。
///
/// 合成出来的仍是一棵普通的 TOML 树 —— 本文件里的 `Config` 形状一个字段都没动，
/// 所以「配置模型」与「配置来源」是两件独立的事（设计原则 P1）。
pub mod layer;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub provider: ProviderConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub runtimes: RuntimesConfig,
    #[serde(default)]
    pub history: HistoryConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
    #[serde(default)]
    pub hook: HookConfig,
    /// `config.toml` 的 `[loop]` 段（循环缝的第二形态，见 `src/agent_loop.rs`）。
    ///
    /// Rust 字段名带下划线是因为 `loop` 是关键字 —— **TOML 那边的键仍然叫 `loop`**
    /// （靠 `rename` 对上），配置文件里看到的就是 `[loop]`。
    #[serde(default, rename = "loop")]
    pub loop_config: LoopConfig,
    /// GUI 专用外观配置。CLI 的 bin 目标也会编译本文件，但读不到这个字段。
    #[allow(dead_code)]
    #[serde(default)]
    pub theme: ThemeConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// 是否以 SSE 流式方式请求（默认 true）。
    /// 开启后思考模型的推理过程可以实时回显；若某个端点不接受
    /// stream / stream_options，设为 false 即回退为等完整响应。
    #[serde(default = "default_true")]
    pub stream: bool,
}

fn default_true() -> bool {
    true
}

impl ProviderConfig {
    pub fn resolve_key(&self) -> Result<String> {
        if let Some(k) = &self.api_key {
            if !k.trim().is_empty() {
                return Ok(k.clone());
            }
        }
        if let Some(env_name) = &self.api_key_env {
            let v = std::env::var(env_name)
                .with_context(|| format!("环境变量 {env_name} 未设置，无法获取 API key"))?;
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
        Err(anyhow!(
            "未配置 API key：请在 config.toml 的 [provider] 中设置 api_key 或 api_key_env"
        ))
    }

    pub fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
// 整表可缺项：`[auth]` 只写 `meta_tools` 时，`plugin_tools` 取代码默认。
// 这与 `[prompt]` / `[history]` / `[store]` / `[theme.metrics]` 早就有的行为一致 ——
// 阶段 5 的分层组合会产出「一张表的局部」（某一层只改一个键），整表必填会让它直接解析失败，
// 而报错信息（missing field）指向的是合成结果，用户会完全摸不着头脑。
#[serde(default)]
pub struct AuthConfig {
    /// ask | auto | deny
    pub meta_tools: String,
    /// ask | auto | deny
    pub plugin_tools: String,
    #[serde(default)]
    pub overrides: HashMap<String, String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { meta_tools: "ask".into(), plugin_tools: "auto".into(), overrides: HashMap::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
// 同上：`[agent]` 由两个 bundle 各写一部分（tools 写超时与截断、prompt-core 写规则文件），
// 整表必填会让这层组合直接失败。
#[serde(default)]
pub struct AgentConfig {
    pub rules_file: Option<String>,
    pub tool_timeout_secs: u64,
    pub max_result_chars: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { rules_file: Some("RULES.md".into()), tool_timeout_secs: 60, max_result_chars: 20000 }
    }
}

#[derive(Debug, Clone, Deserialize)]
// 同上：`[runtimes]` 只写 `python` 时，`node` / `rustc` 取代码默认。
#[serde(default)]
pub struct RuntimesConfig {
    /// 值写成**裸命令**（`python` / `node` / `rustc`）时，宿主会依次尝试
    /// 「应用目录内置运行时 `<app_root>/runtime/<lang>/<exe>`」→「PATH」；
    /// 写成**完整路径**则直接采用，不做任何替换。
    pub python: String,
    pub node: String,
    pub rustc: String,
    /// 覆盖 Python 运行时下载源（默认 CPython 官方嵌入式包，见 `runtimes::DEFAULT_PYTHON_URL`）。
    #[serde(default)]
    pub python_url: Option<String>,
    /// 期望的 SHA-256。留空时：默认源用内置的官方校验值，自定义源则不校验。
    #[serde(default)]
    pub python_sha256: Option<String>,
}

impl Default for RuntimesConfig {
    fn default() -> Self {
        Self {
            python: "python".into(),
            node: "node".into(),
            rustc: "rustc".into(),
            python_url: None,
            python_sha256: None,
        }
    }
}

/// 对话历史落盘。
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConfig {
    /// 是否把对话写入 `<应用根>/history/`。关掉后不再新开文件，
    /// 但已有的历史仍然可读可查看（`/history`、GUI 侧边栏照常工作）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 最多保留多少个对话（按时间，新的优先）；0 = 不限制。
    /// 只在新建对话时裁剪，所以不会删掉正在写的那个。
    #[serde(default = "default_history_keep")]
    pub keep: usize,
}

fn default_history_keep() -> usize {
    200
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { enabled: true, keep: default_history_keep() }
    }
}

/// 对话历史的**存储后端**（阶段 3 抽出的存储缝，见 `src/store.rs`）。
///
/// 本期只有 `"jsonl"` 一种实现：一次对话一个文件，落在 `<应用根>/history/`。
/// **认不出的值会在启动时报错退出**，不会静默退回 jsonl —— 配置写错却照常启动，
/// 表现就是「我明明写了别的后端，怎么还在写 jsonl」，而这类问题最难查。
#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    #[serde(default = "default_store_backend")]
    pub backend: String,
}

    fn default_store_backend() -> String {
        // 默认后端名只在 `store` 模块里写一遍，配置侧引用它 —— 免得两边各写一个字面量。
        crate::store::default_backend().to_string()
    }

impl Default for StoreConfig {
    fn default() -> Self {
        Self { backend: default_store_backend() }
    }
}

/// 系统提示词的组装方式（段的重排 / 禁用 / 追加）。
///
/// 段本身与拼装逻辑在 `src/prompt.rs`；这里只是用户在 `config.toml` 里的那份覆盖。
/// 默认全部为空 = 完全按内置顺序走，与历史版本的硬编码提示词**逐字节相同**。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PromptConfig {
    /// 显式顺序：列出的段 id 按此顺序排到最前，未列出的保持默认序跟在其后。
    ///
    /// 所以「只把 env 挪到最前」只需写 `order = ["env"]`，不必把四段全列一遍。
    #[serde(default)]
    pub order: Vec<String>,
    /// 要禁用的段 id（内置段可用 `core` / `protocol` / `env` / `rules`）。
    #[serde(default)]
    pub disable: Vec<String>,
    /// 追加（或按同 id 覆盖内置）的段。
    #[serde(default)]
    pub segment: Vec<PromptSegmentConfig>,
}

/// `config.toml` 里的一个自定义提示词段。
#[derive(Debug, Clone, Deserialize)]
pub struct PromptSegmentConfig {
    /// 段 id；与内置段同名（`core` / `protocol`）时**整段替换**内置段。
    pub id: String,
    /// 展示名，缺省用 id。
    #[serde(default)]
    pub title: Option<String>,
    pub body: String,
    /// 排序键，缺省 50（排在内置段与运行环境之后）。
    #[serde(default)]
    pub order: Option<i32>,
}

/// 循环钩子（阶段 4 的 Loop 缝，实现见 `src/hook.rs`）。
///
/// 这一段**全空 = 一个钩子都不注册**，`run_loop` 走的路与 Loop 缝之前逐条一致。
///
/// 注意分工：这里管「**放行之后**要不要动东西」，不负责「放不放行」——
/// 拦截工具是授权问题，归 `[auth]`（它的 override 键支持 `fs_*` 这样的通配）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HookConfig {
    /// 给指定工具自动补参数，写法：
    ///
    /// ```toml
    /// [hook.tool_args_default.fs_read_file]
    /// encoding = "utf-8"
    /// ```
    ///
    /// **只补模型没给的那个键**，模型显式传的值一律不动 —— 覆盖模型的显式意图会造出
    /// 「模型说 A、工具收到 B」这种最难查的失效模式。
    #[serde(default)]
    pub tool_args_default: HashMap<String, HashMap<String, toml::Value>>,

    /// 模型请求出错时，宿主**自己**重试几次（配置驱动兜底，不需要任何钩子/组件）。
    ///
    /// 与 `[loop] max_rounds` 同属「不写脚本也能生效」的那一类：组件可以返回
    /// `retry(note)` 做有针对性的一次降级，但没有组件时，偶发的上游抖动照样值得
    /// 重试一次。计数是**每轮**的（`AgentLoop::run` 走一次算一轮），所以不会
    /// 在一段长对话里累积成「越聊越能重试」。
    ///
    /// `0`（默认）= 不重试，错误原样抛出 —— 与阶段 4 逐字节一致。
    #[serde(default)]
    pub max_llm_retries: usize,
}

/// 主循环（阶段 6 · 6a 的循环缝第二形态，实现见 `src/agent_loop.rs`）。
///
/// 阶段 4 的钩子只能在**循环内部**插五个点，骨架本身还是焊死的；这一段决定
/// **用哪个循环骨架**，并顺带给出「上下文裁剪」这个只有循环层才做得出的能力
/// （它要同时动内存与对话文件，还要在裁完重新拼请求）。
///
/// 默认（整段不写）= `standard` + 不裁剪，行为与阶段 5 **逐字节相同**。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    /// 用哪个循环实现。**本期只认 `standard`** —— 别的值在 `Runtime::new` 里
    /// **启动即报错**，不静默退回（与 `[store] backend` 同一条原则：
    /// 写了却不生效是最难查的一类问题）。
    #[serde(rename = "impl")]
    pub implementor: String,
    /// 上下文最多保留多少个「用户轮次」；`0`（默认）= 不裁剪。
    ///
    /// 一轮以一条 `role == "user"` 的消息为界（宿主钩子注入的提示也是 user 消息，
    /// 同样各占一轮）。裁剪只从**最老的整轮**开始丢，且永远保留系统提示那一条 ——
    /// 所以裁完的上下文本身就是一个合法请求体，不会留下「tool 结果还在、
    /// 它的 assistant 已经被丢掉」这种端点会直接拒绝的半截消息。
    pub max_rounds: usize,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self { implementor: default_loop_impl(), max_rounds: 0 }
    }
}

fn default_loop_impl() -> String {
    // 默认实现名只在 `agent_loop` 模块里写一遍，配置侧引用它 —— 免得两边各写一个字面量。
    crate::agent_loop::default_impl().to_string()
}

// ─────────────────────────── 界面外观（GUI 主题）───────────────────────────
//
// 这一段只描述**数据**，不认识 egui。原因有两个：
//   ① `gui::theme::Palette` 里装的是 `egui::Color32`，而 `Color32` 没实现 `Deserialize`；
//   ② 核心库 `aox` 不该依赖 egui（GUI 是可选前端，CLI 用不到）。
// 所以这里放「纯数据颜色 + 逐字段覆盖」，由 GUI 侧换算成 `Color32` 并叠加到内置配色上。

/// 一个颜色值。
///
/// 接受这几种写法（`config.toml` / 主题文件里都能用）：
/// - `"#RRGGBB"`　不透明
/// - `"#RRGGBBAA"`　带 alpha（`AA` 为**不透明度**，`FF` = 完全不透明）
/// - `"#RGB"`　三位简写
/// - `[r, g, b]` / `[r, g, b, a]`　数组形式
///
/// 前导 `#` 可省略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl RgbColor {
    fn parse_hex(s: &str) -> Option<Self> {
        let t = s.trim().trim_start_matches('#');
        // 注意 `get` 会检查字符边界：万一混进非 ASCII，这里直接判为非法而不是 panic。
        let byte = |i: usize| u8::from_str_radix(t.get(i..i + 2)?, 16).ok();
        match t.len() {
            6 => Some(Self { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: 255 }),
            8 => Some(Self { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: byte(6)? }),
            3 => {
                // 三位简写：每个 nibble 复制一遍（`#abc` → `#aabbcc`）
                let nib =
                    |i: usize| -> Option<u8> { Some(u8::from_str_radix(t.get(i..i + 1)?, 16).ok()? * 17) };
                Some(Self { r: nib(0)?, g: nib(1)?, b: nib(2)?, a: 255 })
            }
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for RgbColor {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;

        impl<'de> serde::de::Visitor<'de> for V {
            type Value = RgbColor;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("颜色：\"#RRGGBB\" / \"#RRGGBBAA\" / \"#RGB\" 或 [r, g, b(, a)]")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<RgbColor, E> {
                RgbColor::parse_hex(s).ok_or_else(|| {
                    E::custom(format!("无法解析颜色 {s:?}：应为 #RRGGBB / #RRGGBBAA / #RGB"))
                })
            }

            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<RgbColor, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut v: [u8; 4] = [0, 0, 0, 255];
                let mut n = 0usize;
                while n < 4 {
                    match seq.next_element::<u8>()? {
                        Some(x) => {
                            v[n] = x;
                            n += 1;
                        }
                        None => break,
                    }
                }
                match n {
                    3 | 4 => Ok(RgbColor { r: v[0], g: v[1], b: v[2], a: v[3] }),
                    _ => Err(serde::de::Error::custom("颜色数组需要 3 或 4 个分量（每个 0~255）")),
                }
            }
        }

        d.deserialize_any(V)
    }
}

/// 一套配色的**逐字段覆盖**。只写想改的字段，其余继承内置（纸 / 墨）。
///
/// 字段与 `gui/src/theme.rs` 的 `Palette` 一一对应、顺序一致，方便对照。
/// 全部留空 = 与内置配色**逐像素相同**。
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct PaletteConfig {
    pub bg: Option<RgbColor>,
    pub panel: Option<RgbColor>,
    pub surface: Option<RgbColor>,
    pub surface_hi: Option<RgbColor>,
    pub border: Option<RgbColor>,
    pub border_strong: Option<RgbColor>,
    pub text: Option<RgbColor>,
    pub text_dim: Option<RgbColor>,
    pub text_faint: Option<RgbColor>,
    /// 单色方案里的强调色（亮色模式下几乎全黑，暗色模式下几乎全白）
    pub ink: Option<RgbColor>,
    pub ink_soft: Option<RgbColor>,
    pub user_bg: Option<RgbColor>,
    pub code_bg: Option<RgbColor>,
    /// 行内代码的「芯片」底色（刻意与 `code_bg` 不同深浅）
    pub code_chip: Option<RgbColor>,
    pub quote_bar: Option<RgbColor>,
    /// 反白块底色（错误提示）
    pub reverse_bg: Option<RgbColor>,
    pub on_reverse: Option<RgbColor>,
    /// 悬浮卡片的投影色
    pub shadow: Option<RgbColor>,
}

/// 界面的字号与尺度。每项都留了内置默认值，只写想改的那几项即可。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// 正文字号，全套界面以它为基准
    pub body_size: f32,
    /// 标题（Heading）
    pub heading_size: f32,
    /// 按钮文字
    pub button_size: f32,
    /// 小字
    pub small_size: f32,
    /// 等宽（代码块）
    pub mono_size: f32,
    /// 卡片圆角
    pub card_radius: u8,
    /// 控件间距 `[水平, 垂直]`
    pub item_spacing: [f32; 2],
    /// 按钮内边距 `[水平, 垂直]`
    pub button_padding: [f32; 2],
    /// 交互控件的最小高度
    pub interact_height: f32,
    /// 缩进
    pub indent: f32,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            body_size: 16.0,
            heading_size: 21.0,
            button_size: 15.0,
            small_size: 13.0,
            mono_size: 14.5,
            card_radius: 14,
            item_spacing: [10.0, 10.0],
            button_padding: [14.0, 9.0],
            interact_height: 30.0,
            indent: 22.0,
        }
    }
}

/// 字体文件路径。留空则继续按「环境变量 → 内置候选表」的顺序自动查找。
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct FontsConfig {
    /// 正文 / 中文回退（等价环境变量 `AOX_GUI_FONT`）
    pub cjk: Option<String>,
    /// 粗体（`AOX_GUI_FONT_BOLD`）
    pub bold: Option<String>,
    /// 衬线，次级标题用（`AOX_GUI_FONT_SERIF`）
    pub serif: Option<String>,
    /// 品牌字，首屏大招呼用（`AOX_GUI_FONT_BRAND`）
    pub brand: Option<String>,
    /// 等宽，代码块用（`AOX_GUI_FONT_MONO`）
    pub mono: Option<String>,
}

/// GUI 外观配置：`config.toml` 的 `[theme]`，或用 `--theme <file>` 指定的整份主题文件。
///
/// `--theme` 的文件会**整体取代** `config.toml` 的 `[theme]`（不是逐字段叠加），
/// 所以一份主题文件要么写全，要么期望用内置值补齐。
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct ThemeConfig {
    /// 初始明暗：`"light"` / `"dark"`。
    ///
    /// **优先级最低**：用户上次手动切过主题时，以那份记录为准（见 GUI 的
    /// `.aox-gui-theme`）；只有从未切换过才用这里的值，都没有则用「墨」（暗色）。
    pub mode: Option<String>,
    /// 「纸」（亮色）这一套的覆盖
    #[serde(default)]
    pub light: PaletteConfig,
    /// 「墨」（暗色）这一套的覆盖
    #[serde(default)]
    pub dark: PaletteConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub fonts: FontsConfig,
}

#[allow(dead_code)] // 只有 GUI 会调（`--theme <file>`）；CLI 不认这个参数，但 bin 目标也会编译本文件
impl ThemeConfig {
    /// 从一段 TOML 文本解析主题。**两种写法都接受**：
    ///
    /// - **裸主题**：`mode` / `[metrics]` / `[dark]` … 直接写在顶层；
    /// - **带 `[theme]` 表**：与 `config.toml` 里的写法完全相同。
    ///
    /// 之所以两种都认，是为了「能用也能粘」：`themes/*.toml` 用的是后者，
    /// 所以整段可以原样粘进 `config.toml` 的 `[theme]`；反过来也能把 `config.toml`
    /// 里的 `[theme]` 整段摘出来单独存成主题文件。
    fn from_toml_text(text: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(text)?;
        // `mode` 写在文件开头（也就是 `[theme]` 之外）是很自然的写法，而 TOML 会把它
        // 归到文档根上。先把它取出来，下面表里没有时兜底 —— 免得「明明写了 mode 却不生效」。
        let root_mode = value.get("mode").and_then(|v| v.as_str()).map(str::to_string);
        // 有 `[theme]` 就用它，没有就把整份文件当主题
        let node = value.get("theme").cloned().unwrap_or(value);
        let mut cfg = <ThemeConfig as Deserialize>::deserialize(node)?;
        if cfg.mode.is_none() {
            cfg.mode = root_mode;
        }
        Ok(cfg)
    }

    /// 读取一份独立主题文件（`--theme <path>`）。
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("找不到主题文件 {}", path.display()))?;
        Self::from_toml_text(&text)
            .with_context(|| format!("主题文件 {} 不能当作主题使用", path.display()))
    }
}

impl Config {
    /// 读**单一份** TOML 成 `Config`（不对它做任何分层合成）。
    ///
    /// 阶段 5 之后，仓库内的主路径是 `config::layer::resolve`（profile → bundle → patch
    /// → config.toml 合成）。本函数保留原样，给「手上就是一份 TOML、不需要合成」的调用方用。
    #[allow(dead_code)] // bin 目标里没有调用方；在 lib 目标它是公开 API
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "找不到配置文件 {}。请复制 config.example.toml 为 config.toml 并填写 API 配置。",
                path.display()
            )
        })?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("配置文件 {} 解析失败", path.display()))?;
        Ok(cfg)
    }

    /// selftest 使用的占位配置（不需要真实 provider）
    pub fn dummy() -> Self {
        Self {
            provider: ProviderConfig {
                base_url: String::new(),
                model: String::new(),
                api_key: None,
                api_key_env: None,
                max_tokens: None,
                temperature: None,
                stream: false,
            },
            auth: AuthConfig::default(),
            agent: AgentConfig::default(),
            runtimes: RuntimesConfig::default(),
            history: HistoryConfig::default(),
            store: StoreConfig::default(),
            prompt: PromptConfig::default(),
            hook: HookConfig::default(),
            loop_config: LoopConfig::default(),
            theme: ThemeConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 包一层再解析：TOML 文档不能是裸值，颜色只能作为某个键的值出现。
    #[derive(Deserialize)]
    struct W {
        c: RgbColor,
    }

    fn color(src: &str) -> Option<RgbColor> {
        toml::from_str::<W>(&format!("c = {src}")).ok().map(|w| w.c)
    }

    #[test]
    fn rgb_color_accepts_documented_forms() {
        let a = RgbColor { r: 0x1A, g: 0x2B, b: 0x3C, a: 255 };
        assert_eq!(color("\"#1A2B3C\""), Some(a));
        assert_eq!(color("\"1a2b3c\""), Some(a), "前导 # 可省，大小写不敏感");
        assert_eq!(
            color("\"#1A2B3C80\""),
            Some(RgbColor { r: 0x1A, g: 0x2B, b: 0x3C, a: 0x80 })
        );
        assert_eq!(color("\"#abc\""), Some(RgbColor { r: 0xAA, g: 0xBB, b: 0xCC, a: 255 }));
        assert_eq!(color("[26, 43, 60]"), Some(a));
        assert_eq!(
            color("[26, 43, 60, 128]"),
            Some(RgbColor { r: 26, g: 43, b: 60, a: 128 })
        );
    }

    #[test]
    fn rgb_color_rejects_garbage() {
        for bad in ["\"#12345\"", "\"#GGGGGG\"", "\"红色\"", "[1, 2]", "[]"] {
            assert!(color(bad).is_none(), "{bad} 不该被接受");
        }
    }

    /// 默认 ThemeConfig 必须是**零覆盖**，且尺度默认值等于阶段 2 之前写死在
    /// `gui/src/theme.rs` 的那组常量。这条测试就是「默认外观逐像素不变」的数据层保证。
    #[test]
    fn default_theme_is_zero_override() {
        let t = ThemeConfig::default();
        assert!(t.mode.is_none(), "默认不指定明暗，交给偏好记录 / 内置默认");

        let all_none = |p: &PaletteConfig| {
            p.bg.is_none()
                && p.panel.is_none()
                && p.surface.is_none()
                && p.surface_hi.is_none()
                && p.border.is_none()
                && p.border_strong.is_none()
                && p.text.is_none()
                && p.text_dim.is_none()
                && p.text_faint.is_none()
                && p.ink.is_none()
                && p.ink_soft.is_none()
                && p.user_bg.is_none()
                && p.code_bg.is_none()
                && p.code_chip.is_none()
                && p.quote_bar.is_none()
                && p.reverse_bg.is_none()
                && p.on_reverse.is_none()
                && p.shadow.is_none()
        };
        assert!(all_none(&t.light));
        assert!(all_none(&t.dark));

        let m = &t.metrics;
        assert_eq!(m.body_size, 16.0);
        assert_eq!(m.heading_size, 21.0);
        assert_eq!(m.button_size, 15.0);
        assert_eq!(m.small_size, 13.0);
        assert_eq!(m.mono_size, 14.5);
        assert_eq!(m.card_radius, 14);
        assert_eq!(m.item_spacing, [10.0, 10.0]);
        assert_eq!(m.button_padding, [14.0, 9.0]);
        assert_eq!(m.interact_height, 30.0);
        assert_eq!(m.indent, 22.0);

        assert!(t.fonts.cjk.is_none());
        assert!(t.fonts.bold.is_none());
        assert!(t.fonts.serif.is_none());
        assert!(t.fonts.brand.is_none());
        assert!(t.fonts.mono.is_none());
    }

    #[test]
    fn partial_overrides_only_touch_listed_fields() {
        let t: ThemeConfig = toml::from_str(
            r##"
            mode = "light"

            [metrics]
            body_size = 18.0
            card_radius = 4

            [dark]
            text = "#FF0000"

            [fonts]
            cjk = 'C:\Windows\Fonts\simkai.ttf'
            "##,
        )
        .expect("主题段应能解析");

        assert_eq!(t.mode.as_deref(), Some("light"));
        assert_eq!(t.metrics.body_size, 18.0);
        assert_eq!(t.metrics.card_radius, 4);
        assert_eq!(t.metrics.heading_size, 21.0, "没写的尺度仍取默认");
        assert_eq!(t.dark.text, Some(RgbColor { r: 0xFF, g: 0, b: 0, a: 255 }));
        assert!(t.dark.bg.is_none(), "只改了 text，其余 17 项不受影响");
        assert!(t.light.text.is_none(), "改暗色不该碰到亮色");
        assert_eq!(t.fonts.cjk.as_deref(), Some(r"C:\Windows\Fonts\simkai.ttf"));
        assert!(t.fonts.mono.is_none());
    }

    #[test]
    fn config_theme_section_is_nested_under_theme() {
        let c: Config = toml::from_str(
            r##"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [theme]
            mode = "light"

            [theme.metrics]
            body_size = 20.0

            [theme.dark]
            text = "#123456"
            "##,
        )
        .expect("整份配置应能解析");

        assert_eq!(c.theme.mode.as_deref(), Some("light"));
        assert_eq!(c.theme.metrics.body_size, 20.0);
        assert_eq!(c.theme.metrics.indent, 22.0);
        assert_eq!(c.theme.dark.text, Some(RgbColor { r: 0x12, g: 0x34, b: 0x56, a: 255 }));
        assert!(c.theme.light.text.is_none());
    }

    /// `--theme` 的文件两种写法都要认：裸主题 / 带 `[theme]` 表。
    /// （`themes/*.toml` 用的是后者，所以整段能直接粘进 `config.toml`。）
    #[test]
    fn theme_file_accepts_bare_and_wrapped_shapes() {
        let bare = ThemeConfig::from_toml_text(
            r##"
            mode = "dark"

            [metrics]
            body_size = 19.0

            [dark]
            text = "#00FF00"
            "##,
        )
        .expect("裸主题应能解析");
        assert_eq!(bare.mode.as_deref(), Some("dark"));
        assert_eq!(bare.metrics.body_size, 19.0);
        assert_eq!(bare.dark.text, Some(RgbColor { r: 0, g: 0xFF, b: 0, a: 255 }));

        let wrapped = ThemeConfig::from_toml_text(
            r##"
            [theme]
            mode = "light"

            [theme.metrics]
            body_size = 19.0

            [theme.light]
            text = "#00FF00"
            "##,
        )
        .expect("带 [theme] 表的主题文件应能解析");
        assert_eq!(wrapped.mode.as_deref(), Some("light"));
        assert_eq!(wrapped.metrics.body_size, 19.0);
        assert_eq!(wrapped.light.text, Some(RgbColor { r: 0, g: 0xFF, b: 0, a: 255 }));
        assert!(wrapped.dark.text.is_none());
    }

    /// 交付出去的示例主题文件必须真的能加载 —— 这条测试正是冲着
    /// 「示例文件用了 `[theme]` 嵌套、而加载器只认裸主题」那个 bug 来的。
    #[test]
    fn shipped_theme_files_load() {
        for f in ["themes/paper.toml", "themes/ink.toml", "themes/high-contrast.toml"] {
            let t = ThemeConfig::load(Path::new(f)).unwrap_or_else(|e| panic!("{f} 应能加载: {e:#}"));
            assert!(t.mode.is_some(), "{f} 应声明 mode");
        }

        // 纸 / 墨 是内置配色的完整复制，18 个字段应全部覆盖到
        let paper = ThemeConfig::load(Path::new("themes/paper.toml")).unwrap();
        assert!(paper.light.bg.is_some() && paper.light.text.is_some());
        assert!(paper.light.on_reverse.is_some() && paper.light.shadow.is_some());

        let ink = ThemeConfig::load(Path::new("themes/ink.toml")).unwrap();
        assert!(ink.dark.bg.is_some() && ink.dark.text.is_some());
        assert!(ink.dark.on_reverse.is_some() && ink.dark.shadow.is_some());

        // 高对比两套明暗都定义了（否则另一半会退回内置配色、看着串味）
        let hc = ThemeConfig::load(Path::new("themes/high-contrast.toml")).unwrap();
        assert!(hc.light.bg.is_some() && hc.dark.bg.is_some());
        assert_eq!(hc.metrics.body_size, 17.0);
    }

    /// 老配置（完全没写 `[theme]`）必须照常解析 —— 阶段 2 不能打破既有 config.toml。
    #[test]
    fn config_without_theme_section_still_parses() {        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"
            "#,
        )
        .expect("没有 [theme] 的旧配置必须仍能解析");

        assert!(c.theme.mode.is_none());
        assert!(c.theme.dark.text.is_none());
        assert_eq!(c.theme.metrics.body_size, 16.0);
    }

    /// `[hook.tool_args_default.<工具名>]` 的嵌套写法要能解析成「工具 → 要补哪些键」。
    #[test]
    fn hook_section_parses_nested_tool_args_default() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [hook.tool_args_default.fs_read_file]
            encoding = "utf-8"

            [hook.tool_args_default.fs_write_file]
            create_dirs = true
            "#,
        )
        .expect("带 [hook] 的配置应能解析");

        let d = &c.hook.tool_args_default;
        assert_eq!(d.len(), 2, "两个工具各一条");
        assert_eq!(d["fs_read_file"]["encoding"].as_str(), Some("utf-8"));
        assert_eq!(d["fs_write_file"]["create_dirs"].as_bool(), Some(true));
    }

    /// `[hook] max_llm_retries`（阶段 6 · 6b 的配置驱动兜底）：不写 = 0 = 不重试。
    #[test]
    fn hook_section_parses_max_llm_retries() {
        let p = "[provider]\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\napi_key = \"k\"\n";

        let off: Config = toml::from_str(p).expect("不写 [hook] 应能解析");
        assert_eq!(off.hook.max_llm_retries, 0, "默认 0 = 不重试，与阶段 4 逐字节一致");

        let on: Config =
            toml::from_str(&format!("{p}\n[hook]\nmax_llm_retries = 2\n")).expect("应能解析");
        assert_eq!(on.hook.max_llm_retries, 2);
    }

    /// 老配置（完全没写 `[hook]`）必须照常解析，且钩子表为空 ——
    /// 阶段 4 和阶段 2 一样，不能打破任何一份既有的 config.toml。
    #[test]
    fn config_without_hook_section_still_parses() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"
            "#,
        )
        .expect("没有 [hook] 的旧配置必须仍能解析");
        assert!(c.hook.tool_args_default.is_empty());
    }

    /// `[loop]` 的默认值：standard + 不裁剪。
    /// 这条是「不写 `[loop]` ⇒ 行为与阶段 5 逐字节相同」的数据层保证。
    #[test]
    fn loop_defaults_to_standard_without_trimming() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"
            "#,
        )
        .expect("没有 [loop] 的旧配置必须仍能解析");
        assert_eq!(c.loop_config.implementor, "standard");
        assert_eq!(c.loop_config.max_rounds, 0, "默认不裁剪上下文");
        assert_eq!(LoopConfig::default().implementor, "standard");
    }

    /// `[loop]` 段两个键都要认 —— 顺带钉住「`impl` 这个 TOML 键名不能被 Rust 关键字那件事带偏」。
    #[test]
    fn loop_section_parses_impl_and_max_rounds() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [loop]
            impl = "standard"
            max_rounds = 6
            "#,
        )
        .expect("带 [loop] 的配置应能解析");
        assert_eq!(c.loop_config.implementor, "standard");
        assert_eq!(c.loop_config.max_rounds, 6);
    }

    /// 只写 `max_rounds` 时 `impl` 取默认 —— 与其它各表一样「整表可缺项」。
    #[test]
    fn loop_section_partial_still_parses() {
        let c: Config = toml::from_str(
            r#"
            [provider]
            base_url = "https://example.com/v1"
            model = "m"
            api_key = "k"

            [loop]
            max_rounds = 3
            "#,
        )
        .expect("[loop] 只写一个键也应可解析");
        assert_eq!(c.loop_config.implementor, "standard");
        assert_eq!(c.loop_config.max_rounds, 3);
    }
}
