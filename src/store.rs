//! 存储缝（Store 缝）。
//!
//! 把「对话历史落在哪儿、以什么格式落、怎么读回来」从调用方手里拿走。
//! 调用方（`Agent` / CLI / GUI）只认两个 trait，不再自己拼路径：
//!
//! - [`Store`]：后端级（一个目录 / 一个库）。`Runtime` 持一份 `Arc<dyn Store>`。
//! - [`Session`]：一段**打开的对话**（写入端），由 [`Store::create`] / [`Store::open`] 造出来。
//!
//! **接口上刻意不出现路径**：`dir` / `file_of` 这类「id ↔ 文件名」的映射属于 jsonl 后端的
//! 实现细节（见 [`jsonl`]）。一旦让路径参数出现在 trait 上，将来换后端（例如 sqlite 用一个库
//! 装下所有对话）就实现不出来 —— 那是「抽缝」最容易走歪的一步。所以调用方一律按 **id** 说话。
//!
//! 本期只有 jsonl 一种后端，但 `config.toml [store] backend` 这个入口先立好，
//! 让「换个后端」将来是改配置而不是改代码。

pub mod jsonl;

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::history::{Loaded, Summary, UsageRecord};
use crate::message::Message;

/// 一段打开的对话（写入端）。
///
/// 方法与原先 `history::Log` 一一对应，语义完全相同：追加失败不报错
/// （历史记录不该让正在跑的对话出问题），`rewrite` 要保住活动流与用量行。
pub trait Session: Send {
    /// 对话 id（= 文件名主干）。
    fn id(&self) -> &str;
    /// 落盘位置。**只给界面显示用** —— 调用方不要拿它去拼别的路径。
    ///
    /// `allow(dead_code)`：`src/main.rs` 与 `src/lib.rs` 各自编译同一批模块
    /// （见 lib.rs 开头），CLI 那一份只按 id 说话，用不到它；留着是给
    /// 「显示这次对话落在哪」与测试断言用的（与 `Runtime::add_prompt_segment` 同情形）。
    #[allow(dead_code)]
    fn path(&self) -> &Path;
    /// 追加一条消息（`role == "system"` 会被跳过，系统提示每轮都在变）。
    fn append(&mut self, msg: &Message, is_error: Option<bool>);
    /// 追加一条活动流记录。
    fn append_act(&mut self, act: &Value);
    /// 追加一轮的 token 账。
    fn append_usage(&mut self, rec: &UsageRecord);
    /// 追加一条「本对话的工作区」记录（当前档位 + 绑的根 + 本对话默认档自建的那块地）。
    ///
    /// 每段对话首条消息写一条、每次切档再写一条；读的时候**取最后一条**。
    /// 存它是为了两件事：① 打开旧对话时把当时那块地还回来；
    /// ② 「切到完全访问再切回默认」不必另建一块（见 [`crate::workplace::Record`]）。
    fn append_workplace(&mut self, rec: &crate::workplace::Record);
    /// 用给定消息列表重写整个文件（中断回滚）；活动流与用量原样保留。
    fn rewrite(&mut self, msgs: &[Message]);
}

/// 存储后端（目录级）。
pub trait Store: Send + Sync {
    /// 后端名，即 `config.toml [store] backend` 的那个值。
    fn backend(&self) -> &'static str;
    /// 数据位置（给界面显示「历史存在哪」）。
    fn location(&self) -> &Path;
    /// 新建一段对话并写好头部记录。
    fn create(&self, model: Option<&str>) -> Result<Box<dyn Session>>;
    /// 接上一段已存在的对话（按 id 继续写）。
    fn open(&self, id: &str) -> Result<Box<dyn Session>>;
    /// 列出对话，**最近聊过的在前**。
    fn list(&self, limit: usize) -> Vec<Summary>;
    /// 把「列表里的序号」或「id / id 前缀」解析成一项。
    fn resolve(&self, key: &str) -> Result<Summary>;
    /// 读出整个对话的消息（坏行跳过）。
    fn load(&self, id: &str) -> Result<Vec<Loaded>>;
    /// 读出活动流记录（负载原样返回）。
    ///
    /// `allow(dead_code)`：读时间轴的是 GUI（lib 的消费者），CLI 那一份编译单元
    /// 里没有调用方（见 lib.rs 开头对两套编译的说明）。
    #[allow(dead_code)]
    fn load_acts(&self, id: &str) -> Vec<Value>;
    /// 读出逐轮 token 用量。
    fn load_usage(&self, id: &str) -> Vec<UsageRecord>;
    /// 读出本对话**最后一条**工作区记录；从没写过就是 `None`（旧对话文件都没有）。
    ///
    /// `None` 时的语义由调用方定：`Agent::resume` 选择**不动**当前状态 ——
    /// 不知道那段对话当时用哪块地，就不该替用户猜。
    fn load_workplace(&self, id: &str) -> Option<crate::workplace::Record>;
    /// 删除一段对话，返回被删掉的那一项。
    fn remove(&self, key: &str) -> Result<Summary>;
    /// 只保留最新的 `keep` 个对话（`0` = 不限制），返回删除数。
    ///
    /// 日常路径不需要调用方调它 —— 新建对话时后端会按 `config.toml [history] keep`
    /// 自动裁剪。留在 trait 上是给「手动清理」这类入口预留的，也是「换后端也能保留
    /// 同一个语义」的保证。`allow(dead_code)`：眼下确实没有调用方。
    #[allow(dead_code)]
    fn prune(&self, keep: usize) -> usize;
}

/// 已知的后端名。加后端时同步改这里与 [`backend`]。
pub const BACKENDS: [&str; 1] = ["jsonl"];

/// 默认后端名。`config.toml` 里不写 `[store] backend` 时用的就是它。
pub fn default_backend() -> &'static str {
    BACKENDS[0]
}

/// 造一个默认后端实例，**不会失败**（后端名写死为 [`default_backend`]）。
///
/// 给「配置还没读出来」与「只想读一读历史」的场景用：GUI 在配置读失败时
/// 也要把侧边栏里的历史显示出来，那时手里还没有 `Runtime`，也就没有
/// 配置里的后端名可用 —— 读历史这件事在任何情况下都不该因为别的东西坏了而消失。
pub fn default_store(root: &Path, keep: usize) -> Arc<dyn Store> {
    Arc::new(jsonl::JsonlStore::new(root, keep))
}

/// 按名字造一个后端。
///
/// **认不出的名字直接报错**，不静默退回 jsonl：配置写错却照常启动，表现就是
/// 「我明明写了别的后端，怎么还在写 jsonl」——这类问题最难查。
/// （阶段 2 已经吃过一次「文件写了却不生效」的亏，见 `--theme` 那条。）
pub fn backend(name: &str, root: &Path, keep: usize) -> Result<Arc<dyn Store>> {
    let name = name.trim();
    // 空值 = 没配 = 用默认后端，不是「写错了」，别把它也判成未知。
    if name.is_empty() || name == default_backend() {
        return Ok(default_store(root, keep));
    }
    bail!(
        "未知的存储后端: {name}（当前可用: {}）\n\
         请检查 config.toml 的 [store] backend，或删掉它使用默认值。",
        BACKENDS.join(" / ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 认不出的后端名必须**报错**，而不是静默退回 jsonl。
    ///
    /// 这条守的是「配置写了却不生效」那类最难查的坑：一旦静默回退，用户看到的
    /// 就是「我明明配了别的后端，怎么还在写 jsonl」，而启动日志里一句提示都没有。
    #[test]
    fn unknown_backend_is_an_error() {
        let root = std::env::temp_dir();
        // 用 `.err()` 而不是 `.unwrap_err()`：`Arc<dyn Store>` 不是 `Debug`。
        let e = backend("sqlite", &root, 0)
            .err()
            .expect("未知后端必须报错")
            .to_string();
        assert!(e.contains("sqlite"), "报错要带上写错的那个名字: {e}");
        assert!(e.contains("jsonl"), "报错要告诉用户当前可用的是什么: {e}");
    }

    /// 留空（没配）与显式写默认名，都应当落到 jsonl 后端，位置在 `<root>/history`。
    #[test]
    fn default_backend_resolves_to_jsonl() {
        let root: PathBuf = std::env::temp_dir().join("aox-store-default-test");
        assert_eq!(default_backend(), "jsonl");
        for name in ["", "jsonl", "  jsonl  "] {
            let s = backend(name, &root, 0).unwrap_or_else(|e| panic!("{name:?} 应可用: {e:#}"));
            assert_eq!(s.backend(), "jsonl");
            assert!(s.location().ends_with("history"), "位置: {}", s.location().display());
        }
        assert_eq!(default_store(&root, 0).backend(), "jsonl");
    }
}
