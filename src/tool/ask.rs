//! `ask_user` —— 让模型把「选择题」交给界面，而不是写在正文里等用户打字。
//!
//! # 为什么需要它
//!
//! 模型想请用户做决定时，天然会写成正文里的一句「你选 1 还是 2？」。那句话对宿主
//! **完全不可见** —— 它就是一段普通文本，宿主没有任何机制接住它。后果是：用户必须
//! 自己打字回答，而且他打的字与模型的编号对不对得上全靠运气（「1」是上面第一个
//! 还是下面第一个？），错了谁也不会发现。
//!
//! 所以这里给模型一个**结构化**的出口：`ask_user(question, options[])`。宿主拿到之后
//! 把它画成输入框上方的一排可点选项，等用户点（或打字），再把结果作为工具结果交回去。
//!
//! # 为什么它不在 `Tool::call` 里实现
//!
//! 「问用户」必须拿到 `Ui`，而 `Tool::call(args, rt)` 的签名里没有它 —— 整条工具调用
//! 路径（含插件那条 RPC）都不带界面。授权那条路（`Ui::authorize`）也是因此才走 Agent。
//! 所以这道工具由 [`crate::agent::Agent::authorize_and_execute`] **特判**：
//! 名字命中 [`ASK_TOOL`] 时把 `Ui` 递进 [`interact`]，`call` 永远不会被调到
//!（留一个兜底实现，只为「万一有人绕过 Agent 直接 execute」时给一句能看懂的话）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolKind, ToolOutput};
use crate::runtime::Runtime;
use crate::ui::{ChoiceReply, Ui};

/// 工具名。Agent 特判与这里**引同一个常量** —— 改了名字不会出现「特判失效、
/// 工具被当普通工具跑」那种静默错位（`HOST_FILE_TOOLS` 也是同一个理由）。
pub const ASK_TOOL: &str = "ask_user";

/// 一次最多几个选项。
///
/// 上限不是随手定的：选项槽是输入框上方**一条横排**，超过 6 个就得换行，
/// 而它存在的意义正是「一眼扫完、点一下」。再多就该分两轮问。
pub const MAX_OPTIONS: usize = 6;

pub fn ask_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(AskUser)]
}

pub(crate) struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn name(&self) -> &str {
        ASK_TOOL
    }
    /// 报 `Host`：它是宿主自带的能力（与文件工具同类），不是插件、也不动宿主自身。
    /// `workplace_reject` 那条路对它无害 —— 它的参数里没有 `path`，第一句就返回了。
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    fn description(&self) -> &str {
        "请用户在几个选项里挑一个，并**等他作答**（会暂停当前这轮）。\
         需要用户做决定时用它 —— 在正文里写「你选 1 还是 2？」是没用的：那是纯文本，\
         用户得自己打字，他打的字与你的编号对不对得上也没人保证。\
         选项会自动编号，界面还会附一条「自己输入」。返回值是用户选中的那一项，\
         或者他自己打的一段话（那也是一种有效回答，不要因此重新问一遍）。"
    }
    fn parameters(&self) -> Value {
        parameters()
    }
    async fn call(&self, _args: Value, _rt: &Arc<Runtime>) -> ToolOutput {
        // 正常情况下走不到这里（见模块文档）。真走到了，说明调用方绕开了 Agent，
        // 那句话要能让看日志的人立刻明白缺的是什么。
        ToolOutput::err(format!(
            "{ASK_TOOL} 必须由宿主主循环处理（它需要把界面递进来），不能直接执行"
        ))
    }
}

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "description": "要问用户的一句话。写清在选什么（例如「这份报告按哪种口径统计？」）"
            },
            "options": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 2,
                "maxItems": MAX_OPTIONS,
                "description": "候选答案，2~6 个。**只写正文，不要自己编号** —— 宿主会加 A/B/C…。\
                                每个选项要能独立看懂，不要写「同上」或「选第一个」"
            }
        },
        "required": ["question", "options"]
    })
}

/// 把 `ask_user` 的参数拆成 `(问题, 选项)`。错误给的是**给模型看**的话。
///
/// 抽成纯函数是为了能单测：这里最容易出的错是「悄悄吞掉空选项」，
/// 而吞掉之后模型收到的是一个少了一项的选项槽，双方都不知道少了什么。
pub fn parse(args: &Value) -> Result<(String, Vec<String>), String> {
    let question = args
        .get("question")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{ASK_TOOL} 缺少 question（要给用户一句能看懂的问题）"))?
        .to_string();

    let raw = args
        .get("options")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("{ASK_TOOL} 的 options 必须是字符串数组"))?;

    let mut options: Vec<String> = Vec::with_capacity(raw.len());
    for o in raw {
        let s = o
            .as_str()
            .ok_or_else(|| format!("{ASK_TOOL} 的 options 里有非字符串项：{o}"))?
            .trim()
            .to_string();
        if s.is_empty() {
            return Err(format!("{ASK_TOOL} 的 options 里有空选项 —— 去掉它再调用"));
        }
        options.push(s);
    }
    if options.len() < 2 {
        return Err(format!(
            "{ASK_TOOL} 至少要两个选项（给了 {} 个）。只有一个可能就不用问了，直接做",
            options.len()
        ));
    }
    if options.len() > MAX_OPTIONS {
        // 截断而不是报错：多几个选项不算错，只是界面摆不下。
        options.truncate(MAX_OPTIONS);
    }
    Ok((question, options))
}

/// 选项的编号字符：0 → `A`、1 → `B` …
///
/// 用字母而不是数字：正文里模型常常自己带数字（「1. 2. 3.」），而用户复述时
/// 「我选了 A」不会有歧义。超过 26 个也不会发生（上限 6）。
pub fn letter(i: usize) -> char {
    (b'A' + (i % 26) as u8) as char
}

/// 把用户敲的一行认成「第几项」。认不出返回 `None`。
///
/// 认这些写法：`A` / `a` / `A.` / `a)` / `1` / `1.` —— 界面给的是字母，但用户看着
/// 一串选项敲数字也很自然（模型常在正文里用数字编号）。两种都收。
///
/// 刻意**只认单个字符**（可带一个尾标点）：`ab` 不是选项，`A4 纸` 也不是 ——
/// 那种输入应当原样交给模型当自由回答，而不是猜成 A。猜错的代价是模型拿一个
/// 用户没做过的选择继续往下做，而用户根本没机会发现。
pub fn as_choice(s: &str, n: usize) -> Option<usize> {
    let t = s.trim().trim_end_matches(['.', '。', ')', '）', '、', ':']);
    if t.chars().count() != 1 {
        return None;
    }
    let c = t.chars().next()?;
    if let Some(i) = letter_index(c) {
        return (i < n).then_some(i);
    }
    let d = c.to_digit(10)? as usize;
    // 数字按 **1 基** 认（用户看到的是「1. 2. 3.」）；越界就不认，交给自由回答那条路
    (d >= 1 && d <= n).then(|| d - 1)
}

fn letter_index(c: char) -> Option<usize> {
    let u = c.to_ascii_uppercase();
    u.is_ascii_uppercase().then(|| (u as u8 - b'A') as usize)
}

/// 问一次用户。由 Agent 在拿到 `Ui` 的地方调用（见模块文档）。
pub async fn interact(args: Value, ui: &mut dyn Ui) -> ToolOutput {
    let (question, options) = match parse(&args) {
        Ok(v) => v,
        Err(e) => return ToolOutput::err(e),
    };
    match ui.choose(&question, &options).await {
        ChoiceReply::Picked(i) => match options.get(i) {
            // 把「编号 + 原文」一起回给模型：它后面复述这个选择时，用户能对上。
            Some(text) => ToolOutput::ok(format!("用户选了 {}. {text}", letter(i))),
            None => ToolOutput::err("界面回了一个不存在的选项下标（内部错误），请重新问一次"),
        },
        ChoiceReply::Text(t) => {
            // 特意点明「这也是有效回答」：模型很容易把「用户没点选项」读成「用户没回答」，
            // 于是把同一个问题再问一遍 —— 那是最让人恼火的一种循环。
            ToolOutput::ok(format!("用户没有点选项，直接回答了：{t}"))
        }
        ChoiceReply::Cancelled => ToolOutput::err(
            "用户没有作答（他关掉了选项、或者界面断了）。\
             不要再问同一个问题：要么按你的默认判断继续做，并把「我按 X 做了」说清楚；\
             要么把决定留给用户的下一条消息。",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_normal_question() {
        let args = json!({"question": "  按哪种口径统计？ ", "options": [" 左边 ", "右边"]});
        let (q, o) = parse(&args).unwrap();
        assert_eq!(q, "按哪种口径统计？", "首尾空白要去掉");
        assert_eq!(o, vec!["左边".to_string(), "右边".to_string()]);
    }

    /// 空选项**必须报错**，不能悄悄吞掉：吞掉之后模型收到的是一个少了一格的选项槽，
    /// 双方都不知道少了什么，而用户点的编号与模型以为的编号从此错位。
    #[test]
    fn empty_or_non_string_options_are_rejected_not_swallowed() {
        let e = parse(&json!({"question": "q", "options": ["a", "  "]})).unwrap_err();
        assert!(e.contains("空选项"), "{e}");
        let e = parse(&json!({"question": "q", "options": ["a", 1]})).unwrap_err();
        assert!(e.contains("非字符串"), "{e}");
    }

    #[test]
    fn a_single_option_is_not_a_question() {
        let e = parse(&json!({"question": "q", "options": ["只有一个"]})).unwrap_err();
        assert!(e.contains("至少要两个"), "{e}");
    }

    #[test]
    fn missing_question_is_an_error() {
        let e = parse(&json!({"options": ["a", "b"]})).unwrap_err();
        assert!(e.contains("question"), "{e}");
    }

    /// 超过上限是**截断**而不是报错：多给几个不算错，只是界面摆不下。
    #[test]
    fn too_many_options_are_truncated_not_rejected() {
        let opts: Vec<String> = (0..9).map(|i| format!("选项{i}")).collect();
        let (_, o) = parse(&json!({"question": "q", "options": opts})).unwrap();
        assert_eq!(o.len(), MAX_OPTIONS);
        assert_eq!(o[0], "选项0", "截断要留前面几个，不是随机丢");
    }

    #[test]
    fn letters_start_at_a() {
        assert_eq!(letter(0), 'A');
        assert_eq!(letter(1), 'B');
        assert_eq!(letter(5), 'F');
    }

    /// 认编号要**准**，不能贪：认错的代价是模型拿一个用户没做过的选择继续往下做。
    #[test]
    fn choice_parsing_accepts_letters_and_numbers_but_nothing_fuzzy() {
        let n = 3;
        for (input, want) in [
            ("A", 0usize),
            ("a", 0),
            ("b", 1),
            ("C", 2),
            ("B.", 1),
            ("c)", 2),
            ("  1  ", 0),
            ("2.", 1),
            ("3、", 2),
        ] {
            assert_eq!(as_choice(input, n), Some(want), "{input:?} 应当认成第 {want} 项");
        }
        // 这些**不该**被认成选项 —— 它们该原样当自由回答交给模型
        for input in ["", "ab", "A4 纸", "第 1 个", "A B", "是", "0", "4"] {
            assert_eq!(as_choice(input, n), None, "{input:?} 不该被认成选项");
        }
    }
}
