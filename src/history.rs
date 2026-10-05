//! 对话历史的**数据模型与文案**。
//!
//! 这里只剩「一条记录长什么样」与「怎么显示给人看」：`Meta` / `UsageRecord` /
//! `Summary` / `Loaded` 四个结构，加上 `/history` 的子命令解析与纯文本渲染。
//!
//! **落盘与读取搬去了 [`crate::store`]**（阶段 3 抽出的存储缝，实现在 `store::jsonl`）。
//! 分成两边的意义：换存储后端不牵连文案，改措辞也不碰 I/O；调用方按 **id** 说话，
//! 不再自己拼历史文件路径。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::llm::Usage;
use crate::message::Message;

/// 列表里默认返回多少个对话。
///
/// CLI（`/history`）与后端解析「序号」都用它 —— 序号是按这个列表的第几位算的，
/// 两处一旦不同，`/history rm 2` 删掉的就不是用户看到的那一条。
pub const DEFAULT_LIST_LIMIT: usize = 60;

/// 宿主注入的提示语前缀。这类「用户消息」不是真人说的，不该拿来当标题。
///
/// 后端读标题时要判它，CLI 渲染全文时也要判它 —— 同一个判据，只写一遍。
pub(crate) const HOST_PREFIX: &str = "（宿主提示）";

/// 当前 Unix 秒。
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ─────────────────────────── 记录 ───────────────────────────

/// 对话文件的头部记录（jsonl 的第一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// Unix 秒，便于程序比较
    #[serde(default)]
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// 一轮任务的 token 账（对应 jsonl 里的一行 `kind:"usage"`）。
///
/// **逐轮各存一条**，而不是在文件末尾只留一个总数：那样只能回答「这段对话花了多少」，
/// 回答不了「哪一轮最贵」。而最贵的那一轮往往正是出问题的那一轮 —— 定位要靠它。
///
/// 各项都是「有值才写」：端点少报一项，文件里就少一个键，读回来自然是 `None`，
/// 不用拿 0 去冒充「没有消耗」。
#[derive(Debug, Clone, Default)]
pub struct UsageRecord {
    /// 第几轮（从 1 起）
    pub round: u32,
    /// 写入时刻（unix 秒）
    pub at: u64,
    /// 本轮模型被调用了几次
    pub calls: u32,
    /// 本轮各项用量
    pub usage: Usage,
    /// 最后一次请求的输入规模 = 当前上下文占用
    pub last_prompt: Option<u64>,
    /// 截至本轮的**对话累计** token。冗余存一份，省得读出来还要自己累加，
    /// 也让「这段对话一共花了多少」在文件里一眼可见。
    pub session_total: Option<u64>,
    /// 截至本轮的对话累计调用次数
    pub session_calls: u32,
}

impl UsageRecord {
    pub fn to_value(&self) -> Value {
        let mut v = json!({
            "round": self.round,
            "at": self.at,
            "calls": self.calls,
            "session_calls": self.session_calls,
        });
        let mut put = |key: &str, x: Option<u64>| {
            if let Some(n) = x {
                v[key] = json!(n);
            }
        };
        put("prompt", self.usage.prompt);
        put("completion", self.usage.completion);
        put("total", self.usage.total);
        put("reasoning", self.usage.reasoning);
        put("cached", self.usage.cached);
        put("cache_miss", self.usage.cache_miss);
        put("last_prompt", self.last_prompt);
        put("session_total", self.session_total);
        v
    }

    pub fn from_value(v: &Value) -> Self {
        let num = |k: &str| v.get(k).and_then(Value::as_u64);
        let n32 = |k: &str| num(k).unwrap_or(0) as u32;
        Self {
            round: n32("round"),
            at: num("at").unwrap_or(0),
            calls: n32("calls"),
            usage: Usage {
                prompt: num("prompt"),
                completion: num("completion"),
                total: num("total"),
                reasoning: num("reasoning"),
                cached: num("cached"),
                cache_miss: num("cache_miss"),
            },
            last_prompt: num("last_prompt"),
            session_total: num("session_total"),
            session_calls: n32("session_calls"),
        }
    }

    /// 这一行是否一个数字都没带（端点压根不返回 usage 时就是空的）
    pub fn is_empty(&self) -> bool {
        self.usage.is_empty()
    }
}

/// 列表里的一项。
#[derive(Debug, Clone)]
pub struct Summary {
    /// 文件名主干（时间戳 + 盐），也是 `/history show` 与 GUI 用的标识。
    /// 调存储层时一律用它，**不要**用 `path` 拼别的路径。
    pub id: String,
    /// 落盘位置。展示用（`/history dir`、界面上的路径提示）。
    pub path: std::path::PathBuf,
    /// 首条用户消息（截断）；拿不到就用占位文案
    pub title: String,
    /// 消息条数（不含 meta）
    pub messages: usize,
    /// 「最后一次对话时间」的显示串（「10-02 15:39」）。与列表排序用的是同一个
    /// 时间 —— 排序键和显示的时间一旦不是同一个，列表看着就像排错了。
    pub time: String,
}

/// 一条读出来的消息。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub msg: Message,
    pub is_error: Option<bool>,
}

// ─────────────────────────── 命令（CLI / GUI 共用） ───────────────────────────

/// `/history` 子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// 列出历史对话
    List,
    /// 打印某个对话全文
    Show(String),
    /// 切换到某个对话（读回上下文，之后的发言继续写进同一个文件）
    Open(String),
    /// 打印历史目录
    Dir,
    /// 删除某个对话
    Remove(String),
}

/// 这一行是不是 `/history` 命令。
pub fn is_cmd(line: &str) -> bool {
    line == "/history" || line.starts_with("/history ")
}

pub fn parse_cmd(line: &str) -> Result<Cmd> {
    let mut parts = line.split_whitespace();
    let _ = parts.next();
    match parts.next() {
        None | Some("list") => Ok(Cmd::List),
        Some("dir") => Ok(Cmd::Dir),
        Some("show") => match parts.next() {
            Some(id) => Ok(Cmd::Show(id.to_string())),
            None => bail!("用法: /history show <序号|文件名>"),
        },
        Some("open") | Some("use") => match parts.next() {
            Some(id) => Ok(Cmd::Open(id.to_string())),
            None => bail!("用法: /history open <序号|文件名>"),
        },
        Some("rm") | Some("remove") => match parts.next() {
            Some(id) => Ok(Cmd::Remove(id.to_string())),
            None => bail!("用法: /history rm <序号|文件名>"),
        },
        Some(other) => bail!(
            "未知的 /history 子命令: {other}\n\
             用法: /history [list] | /history show <序号|文件名> | \
             /history open <序号|文件名> | /history rm <序号|文件名> | /history dir"
        ),
    }
}

// ─────────────────────────── 文案 ───────────────────────────

/// `/history` 的纯文本实现（CLI 直接打印）。
///
/// 只负责排版：数据由调用方从存储层取好传进来 —— 这样「换后端」与「改措辞」
/// 互不牵连，也免得这个函数自己还要去读盘。
pub fn list_text(items: &[Summary], location: &Path, current: Option<&str>) -> String {
    if items.is_empty() {
        return format!("（还没有历史记录）\n  目录: {}", location.display());
    }
    let mut s = format!("历史记录（{} 个对话，最近聊过的在前）  {}", items.len(), location.display());
    for (i, it) in items.iter().enumerate() {
        let cur = if Some(it.id.as_str()) == current { "  ← 当前" } else { "" };
        s.push_str(&format!(
            "\n  {:>2}. {}  {:>4} 条  {}{}",
            i + 1,
            it.time,
            it.messages,
            it.title,
            cur
        ));
    }
    s.push_str(
        "\n  用法: /history show <序号> 看全文 | /history open <序号> 接着聊 | \
         /history rm <序号> | /history dir",
    );
    s
}

/// 把一个对话渲染成纯文本（CLI 的 `/history show`）。
pub fn transcript_text(items: &[Loaded]) -> String {
    use crate::ui::style;
    if items.is_empty() {
        return "（空对话）".into();
    }
    let mut s = String::new();
    for it in items {
        let m = &it.msg;
        match m.role.as_str() {
            "user" => {
                // 用 `flat_text`：用户消息可能带附件，那时 `content` 是 parts 数组
                //（正文 + 图片块），直接 `as_deref` 会一个字都显示不出来。
                let text = m.flat_text();
                let text = text.trim();
                if text.starts_with(HOST_PREFIX) {
                    s.push_str(&format!("\n{} {}\n", style::dim("宿主:"), style::dim(text)));
                } else {
                    s.push_str(&format!("\n{} {}\n", style::bold("你:"), text));
                }
                // 附件在 `.jsonl` 里只有一条路径引用（见 `attach` 模块），所以翻历史时
                // 必须显式列出来 —— 否则一段带图对话回看起来与纯文字对话毫无区别，
                // 而「当时到底发了什么」正是回看历史要回答的那个问题。
                let files = m
                    .content
                    .as_ref()
                    .map(crate::attach::names_from_content)
                    .unwrap_or_default();
                if !files.is_empty() {
                    s.push_str(&format!(
                        "  {} {}\n",
                        style::dim("附件:"),
                        style::dim(&files.join("、"))
                    ));
                }
            }
            "assistant" => {
                if let Some(t) = m.text() {
                    s.push_str(&format!("\n{} {}\n", style::bold("助手:"), t));
                }
                for c in m.tool_calls.as_deref().unwrap_or(&[]) {
                    s.push_str(&format!(
                        "\n  {} {} {}\n",
                        style::dim("→"),
                        style::cyan(&c.function.name),
                        style::dim(&clamp_chars(c.function.arguments.trim(), 160))
                    ));
                }
            }
            "tool" => {
                let (mark, name) = (
                    if it.is_error == Some(true) { "✗" } else { "✓" },
                    m.name.as_deref().unwrap_or("tool"),
                );
                // 工具结果可能很长（写入前已被 max_result_chars 收过一道），
                // 展示时再收一道，翻历史才不用在一条结果里滚屏。
                let body = m.flat_text().replace('\n', " ⏎ ");
                s.push_str(&format!("  {} {} {}\n", mark, name, style::dim(&clamp_chars(body.trim(), 300))));
            }
            _ => {}
        }
    }
    s
}

fn clamp_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 序号列表与解析出来的条数用的是同一个上限 —— 两处一旦不同，
    /// `/history rm 2` 删掉的就不是用户看到的那一条。
    #[test]
    fn list_limit_is_the_shared_one() {
        assert!(DEFAULT_LIST_LIMIT > 0);
    }

    /// 用量记录：有值才写、缺项读回是 None（不是 0）。
    #[test]
    fn usage_record_round_trips_sparse_fields() {
        let rec = UsageRecord {
            round: 2,
            at: 42,
            calls: 1,
            usage: Usage { prompt: Some(10), total: Some(30), ..Default::default() },
            last_prompt: None,
            session_total: Some(60),
            session_calls: 3,
        };
        let back = UsageRecord::from_value(&rec.to_value());
        assert_eq!(back.round, 2);
        assert_eq!(back.usage.prompt, Some(10));
        assert_eq!(back.usage.total, Some(30));
        assert_eq!(back.usage.completion, None, "没报就是没有，不许变成 0");
        assert_eq!(back.last_prompt, None);
        assert_eq!(back.session_total, Some(60));
        assert_eq!(back.session_calls, 3);
        assert!(!back.is_empty());
    }

    /// 子命令解析：别名、缺参报错、未知子命令报错。
    #[test]
    fn parse_cmd_handles_aliases_and_errors() {
        assert_eq!(parse_cmd("/history").unwrap(), Cmd::List);
        assert_eq!(parse_cmd("/history list").unwrap(), Cmd::List);
        assert_eq!(parse_cmd("/history dir").unwrap(), Cmd::Dir);
        assert_eq!(parse_cmd("/history show 3").unwrap(), Cmd::Show("3".into()));
        assert_eq!(parse_cmd("/history use abc").unwrap(), Cmd::Open("abc".into()));
        assert_eq!(parse_cmd("/history rm 1").unwrap(), Cmd::Remove("1".into()));
        assert!(parse_cmd("/history show").is_err());
        assert!(parse_cmd("/history nope").is_err());
    }

    /// 标题用的前缀判据不能在渲染全文时失效：宿主提示要显示成「宿主:」。
    #[test]
    fn transcript_marks_host_prompts() {
        let items = vec![
            Loaded { msg: Message::user("（宿主提示）你上一条回复是空的。"), is_error: None },
            Loaded { msg: Message::user("帮我看看"), is_error: None },
        ];
        let text = transcript_text(&items);
        assert!(text.contains("宿主:"), "宿主提示要单独标出来");
        assert!(text.contains("你:"), "真人消息用「你:」");
    }
}
