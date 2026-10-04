//! 宿主内置的**文件工具** —— 沙箱方案 ③ 的落地形态。
//!
//! 为什么要宿主自己提供，而不是继续让模型写 `fs` 插件：
//!
//! 模型自己写的文件插件**只能在它自己的代码里自律**，宿主看不见它拿什么路径去干了什么。
//! 而这三个（四个）工具的参数与实现都在宿主手里 —— 所以「路径必须先过工作区判定」
//! 这件事**无法被绕过**（除非模型不用它们、改自己造插件，那属于另一类风险，
//! 见 `docs/SANDBOX.md` 的威胁模型）。
//!
//! 判定在两处做，是刻意的纵深：
//! 1. `Agent::authorize_and_execute` —— 所有工具调用的唯一收口，**早于授权**。
//!    在那儿拦下的好处是能顺带给用户发一条 `Warn`（工具拿不到 `Ui`）。
//! 2. 每个工具自己的 `call` 开头 —— 防止有人绕过循环直接 `Runtime::execute`
//!    （`selftest` 就是这么调的）。少一层的话，那条路径上判定是空的。
//!
//! 两处都走 `workplace` 模块的**同一套判定**（读用 `check_read`、写删用 `check`，
//! 分野见 [`crate::workplace::Access`]）与同一份文案（[`crate::workplace::Reject`]），
//! 所以不存在「两套规则」。
//!
//! **读不设限、写删受限**（2026-10-04 定的语义）：`read_file` / `list_dir` 只把路径
//! 规格化就去开文件 —— 整个设备都能读；`write_file` / `remove` 才真正过工作区边界。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolKind, ToolOutput, HOST_FILE_TOOLS};
use crate::runtime::Runtime;
use crate::workplace::PATH_ARG;

/// 四个宿主文件工具。
pub fn host_file_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool),
        Arc::new(WriteFileTool),
        Arc::new(ListDirTool),
        Arc::new(RemoveTool),
    ]
}

/// 单次读文件的字节上限。超过就截断并**明说截断了** ——
/// 悄悄截断会让模型基于残缺内容下结论，是「静默出错」里最坏的一种。
const READ_LIMIT: usize = 200_000;
/// 列目录时的条目上限，理由同上。
const LIST_LIMIT: usize = 300;

/// 判定一次「**落盘**」的 `path` 参数（写 / 删），顺便把「越界」统一成给模型的契约文本。
fn guard(rt: &Arc<Runtime>, raw: &str) -> Result<std::path::PathBuf, ToolOutput> {
    rt.workplace.check(raw).map_err(|r| ToolOutput::err(r.model_text()))
}

/// 判定一次「**只读**」的 `path` 参数（读文件 / 列目录）。
///
/// 按 [`crate::workplace::Access::Read`] 的语义，读**不受工作区限制**，所以这里
/// 不会返回错误，只把路径规格化后交出去。**刻意与 [`guard`] 分开写**：让「读和写
/// 不是同一档」这件事在调用点上一眼可见 —— 若两者共用一个 `guard`，哪天有人给读也
/// 套上边界，代码上完全看不出变化。
fn guard_read(rt: &Arc<Runtime>, raw: &str) -> std::path::PathBuf {
    // `check_read` 恒为 `Ok`（读从不被拒）；真出了意外也不该 panic，退回原样路径。
    rt.workplace.check_read(raw).unwrap_or_else(|_| std::path::PathBuf::from(raw))
}

/// 取必填的 `path` 参数。
fn path_arg(args: &Value) -> Result<&str, ToolOutput> {
    args.get(PATH_ARG)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolOutput::err(format!("缺少参数 {PATH_ARG}（必须是字符串路径）")))
}

// ---------------------------------------------------------------- read_file

pub(crate) struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[0]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    fn description(&self) -> &str {
        "读取一个文本文件并返回其内容（宿主提供）。读取不受工作区限制 —— 设备上的任何文件都能读。"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": "要读取的文件路径（绝对路径；读取不受工作区限制）"},
                "max_bytes": {"type": "integer", "description": "最多读取多少字节，默认 200000"}
            },
            "required": [PATH_ARG]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let raw = match path_arg(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        // 读走 `guard_read`：**不受工作区限制**（2026-10-04 语义）。
        let path = guard_read(rt, raw);
        if path.is_dir() {
            return ToolOutput::err(format!(
                "{} 是一个目录，不是文件。要看里面有什么请用 list_dir。",
                path.display()
            ));
        }
        let limit = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(READ_LIMIT)
            .min(READ_LIMIT);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("读取 {} 失败: {e}", path.display())),
        };
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return ToolOutput::err(format!(
                "{} 看起来是二进制文件（含 NUL 字节），本工具只读文本。",
                path.display()
            ));
        }
        let truncated = bytes.len() > limit;
        // 截断可能落在多字节字符中间，所以用 lossy 而不是直接 from_utf8 失败
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).into_owned();
        let mut out = format!("{}（{} 字节）\n", path.display(), bytes.len());
        if truncated {
            out.push_str(&format!(
                "⚠️ 文件超过 {limit} 字节，**下面只是前 {limit} 字节**，不是全文。\n"
            ));
        }
        out.push_str(&body);
        ToolOutput::ok(out)
    }
}

// ---------------------------------------------------------------- write_file

pub(crate) struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[1]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    fn description(&self) -> &str {
        "把一个字符串写进文本文件（覆盖写，宿主提供，受工作区限制）。路径必须是工作区内的绝对路径；父目录默认自动创建。"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": "要写入的文件路径（绝对路径，且在工作区内）"},
                "content": {"type": "string", "description": "文件内容（整份覆盖）"},
                "create_dirs": {"type": "boolean", "description": "父目录不存在时是否自动创建，默认 true"}
            },
            "required": [PATH_ARG, "content"]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let raw = match path_arg(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let path = match guard(rt, raw) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::err("缺少参数 content（必须是字符串）"),
        };
        if path.is_dir() {
            return ToolOutput::err(format!("{} 是一个目录，不能当文件写。", path.display()));
        }
        let create_dirs = args.get("create_dirs").and_then(|v| v.as_bool()).unwrap_or(true);
        if create_dirs {
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        return ToolOutput::err(format!("创建父目录 {} 失败: {e}", parent.display()));
                    }
                }
            }
        } else if let Some(parent) = path.parent() {
            if !parent.is_dir() {
                return ToolOutput::err(format!(
                    "父目录 {} 不存在（create_dirs = false，所以没有自动创建）",
                    parent.display()
                ));
            }
        }
        let existed = path.exists();
        match std::fs::write(&path, content.as_bytes()) {
            Ok(()) => ToolOutput::ok(format!(
                "已{} {}（{} 字节）",
                if existed { "覆盖" } else { "创建" },
                path.display(),
                content.len()
            )),
            Err(e) => ToolOutput::err(format!("写入 {} 失败: {e}", path.display())),
        }
    }
}

// ---------------------------------------------------------------- list_dir

pub(crate) struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[2]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    fn description(&self) -> &str {
        "列出目录内容（宿主提供）。列出不受工作区限制。不传 path 时列出当前工作区根。目录名带 `/` 后缀。"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": "要列出的目录（绝对路径）；省略 = 工作区根"}
            },
            "required": []
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        // 不传 path = 工作区根（不受限档位下退化成应用根）。
        // 这条路径**模型没有指定任何位置**，所以不需要判定 —— 根是宿主自己给的。
        let dir = match args.get(PATH_ARG).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
            // 列目录也是「只读」：走 `guard_read`，不受工作区限制。
            Some(raw) => guard_read(rt, raw),
            None => match rt.workplace.active_root() {
                Some(p) => p,
                None => rt.root.clone(),
            },
        };
        if !dir.exists() {
            return ToolOutput::err(format!("{} 不存在", dir.display()));
        }
        if !dir.is_dir() {
            return ToolOutput::err(format!("{} 不是目录。要读文件请用 read_file。", dir.display()));
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(format!("列出 {} 失败: {e}", dir.display())),
        };
        // 目录在前、同类按名字排 —— 读起来才有结构，而不是文件系统的返回顺序
        let mut items: Vec<(bool, String, u64)> = Vec::new();
        for e in entries.flatten() {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            items.push((is_dir, e.file_name().to_string_lossy().into_owned(), size));
        }
        items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = items.len();
        let mut out = format!("{}（{} 项）\n", dir.display(), total);
        for (is_dir, name, size) in items.iter().take(LIST_LIMIT) {
            if *is_dir {
                out.push_str(&format!("  {name}/\n"));
            } else {
                out.push_str(&format!("  {name}  ({size} 字节)\n"));
            }
        }
        if total > LIST_LIMIT {
            out.push_str(&format!("  …另有 {} 项未列出（上限 {LIST_LIMIT}）\n", total - LIST_LIMIT));
        }
        ToolOutput::ok(out)
    }
}

// ---------------------------------------------------------------- remove

pub(crate) struct RemoveTool;

#[async_trait]
impl Tool for RemoveTool {
    fn name(&self) -> &str {
        HOST_FILE_TOOLS[3]
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Host
    }
    fn description(&self) -> &str {
        "删除一个文件或空目录（宿主提供，受工作区限制）。删非空目录必须显式 recursive = true。"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                PATH_ARG: {"type": "string", "description": "要删除的路径（绝对路径，且在工作区内）"},
                "recursive": {"type": "boolean", "description": "是否递归删除非空目录，默认 false"}
            },
            "required": [PATH_ARG]
        })
    }

    async fn call(&self, args: Value, rt: &Arc<Runtime>) -> ToolOutput {
        let raw = match path_arg(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let path = match guard(rt, raw) {
            Ok(p) => p,
            Err(e) => return e,
        };
        // 别把工作区根自己删了 —— 那会让本对话剩下所有文件操作都无处可去。
        // 这一条与权限无关，纯粹是「删了就没法继续干活」的保护。
        if rt.workplace.active_root().as_deref() == Some(path.as_path()) {
            return ToolOutput::err(format!(
                "{} 是当前工作区根，不能删除它自己。要清理内容请删里面的条目。",
                path.display()
            ));
        }
        if !path.exists() {
            return ToolOutput::err(format!("{} 不存在", path.display()));
        }
        let recursive = args.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false);
        let is_dir = path.is_dir();
        if is_dir {
            if !recursive {
                // 非空目录不给删：这是「一次手滑删掉一整棵树」的唯一闸门
                let non_empty = std::fs::read_dir(&path).map(|mut it| it.next().is_some()).unwrap_or(false);
                if non_empty {
                    return ToolOutput::err(format!(
                        "{} 是非空目录。要真的递归删掉它，请显式传 recursive = true。",
                        path.display()
                    ));
                }
            }
            let r = if recursive { std::fs::remove_dir_all(&path) } else { std::fs::remove_dir(&path) };
            match r {
                Ok(()) => ToolOutput::ok(format!(
                    "已删除目录 {}（{}）",
                    path.display(),
                    if recursive { "递归" } else { "空目录" }
                )),
                Err(e) => ToolOutput::err(format!("删除目录 {} 失败: {e}", path.display())),
            }
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => ToolOutput::ok(format!("已删除文件 {}", path.display())),
                Err(e) => ToolOutput::err(format!("删除文件 {} 失败: {e}", path.display())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WorkplaceConfig};
    use crate::runtime::Runtime;

    /// 造一个「固定工作区」的运行时，用来验证工具确实受边界约束。
    ///
    /// 必须真的把工具注册进去：`Runtime::execute` 是按注册表找人，绕过注册表就没得测。
    /// 这正是本模块与 `workplace` 模块单测的分工 —— 那边测判定，这边测「判定真的接在
    /// 工具调用路径上」。
    async fn rt_with_ws(dir: &std::path::Path) -> Arc<Runtime> {
        let mut cfg = Config::dummy();
        cfg.workplace = WorkplaceConfig { mode: "fixed".into(), root: Some(dir.to_string_lossy().into()) };
        // 解释器解析走不通也无所谓：本模块的测试不启插件
        let rt = Runtime::new(cfg, dir.to_path_buf(), dir.join("plugins")).expect("运行时应当能建起来");
        for t in host_file_tools() {
            rt.register(t).await.expect("注册宿主文件工具应当成功");
        }
        rt
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aox-hostfs-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let d = tmp("rw");
        let rt = rt_with_ws(&d).await;
        let f = d.join("sub").join("a.txt");

        let w = rt.execute(HOST_FILE_TOOLS[1], json!({PATH_ARG: f.to_string_lossy(), "content": "你好\n"})).await;
        assert!(!w.is_error, "{}", w.content);
        assert!(f.exists(), "父目录应当被自动创建");

        let r = rt.execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("你好"), "内容应当读回来：{}", r.content);
    }

    /// **写**越界必须被拦，且给模型的那份要带上三段契约。
    ///
    /// 注意是「写」：读不受工作区限制（见 `read_outside_is_allowed_but_remove_is_not`），
    /// 所以这条只能挂在 `write_file` 上 —— 挂在 `read_file` 上会变成一条永远绿的假测试。
    #[tokio::test]
    async fn outside_write_is_refused_with_the_contract() {
        let d = tmp("outside");
        let rt = rt_with_ws(&d).await;
        let bad = d.parent().unwrap().join("aox-hostfs-evil.txt");
        let out = rt.execute(HOST_FILE_TOOLS[1], json!({PATH_ARG: bad.to_string_lossy(), "content": "x"})).await;
        assert!(out.is_error);
        assert!(!bad.exists(), "越界的写入绝不能真的发生");
        assert!(out.content.contains("权限边界"), "{}", out.content);
        assert!(out.content.contains("不要重试"), "{}", out.content);
        assert!(out.content.contains("转述给用户"), "{}", out.content);
    }

    /// 新语义（2026-10-04）：**读不受工作区限制** —— 区外的文件读得到、区外的目录列得出；
    /// 而**删**仍然受限。两件事一起钉，免得哪天又被「统一」回一档
    /// （只钉一边的话，「读通过了」也可能只是读压根没走判定，那是漏了，不是语义对）。
    #[tokio::test]
    async fn read_outside_is_allowed_but_remove_is_not() {
        let d = tmp("rdout");
        let rt = rt_with_ws(&d).await;
        // 工作区**外面**的一个文件与目录
        let out_dir = d.parent().unwrap().join("aox-hostfs-outside-dir");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();
        let f = out_dir.join("note.txt");
        std::fs::write(&f, "外面的内容").unwrap();

        // 读：放行
        let r = rt.execute(HOST_FILE_TOOLS[0], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(!r.is_error, "读区外文件必须放行（新语义）：{}", r.content);
        assert!(r.content.contains("外面的内容"), "{}", r.content);

        // 列目录：同样放行
        let l = rt.execute(HOST_FILE_TOOLS[2], json!({PATH_ARG: out_dir.to_string_lossy()})).await;
        assert!(!l.is_error, "列区外目录必须放行：{}", l.content);
        assert!(l.content.contains("note.txt"), "{}", l.content);

        // 删：仍然被拦，且文件必须还在
        let rm = rt.execute(HOST_FILE_TOOLS[3], json!({PATH_ARG: f.to_string_lossy()})).await;
        assert!(rm.is_error, "删区外文件必须被拦：{}", rm.content);
        assert!(f.exists(), "越界的删除绝不能真的发生");
        assert!(rm.content.contains("权限边界"), "{}", rm.content);

        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[tokio::test]
    async fn non_empty_dir_needs_recursive() {
        let d = tmp("rm");
        let rt = rt_with_ws(&d).await;
        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), "x").unwrap();

        let denied = rt.execute(HOST_FILE_TOOLS[3], json!({PATH_ARG: sub.to_string_lossy()})).await;
        assert!(denied.is_error, "非空目录不该被一次手滑删掉");
        assert!(sub.exists());

        let forced = rt
            .execute(HOST_FILE_TOOLS[3], json!({PATH_ARG: sub.to_string_lossy(), "recursive": true}))
            .await;
        assert!(!forced.is_error, "{}", forced.content);
        assert!(!sub.exists());
    }

    #[tokio::test]
    async fn workplace_root_itself_is_not_removable() {
        let d = tmp("root");
        let rt = rt_with_ws(&d).await;
        let out = rt.execute(HOST_FILE_TOOLS[3], json!({PATH_ARG: d.to_string_lossy(), "recursive": true})).await;
        assert!(out.is_error, "工作区根不能被删");
        assert!(d.exists());
    }

    #[tokio::test]
    async fn list_dir_defaults_to_the_workplace_root() {
        let d = tmp("ls");
        std::fs::write(d.join("a.txt"), "12345").unwrap();
        std::fs::create_dir_all(d.join("folder")).unwrap();
        let rt = rt_with_ws(&d).await;
        let out = rt.execute(HOST_FILE_TOOLS[2], json!({})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("folder/"), "目录应带 / 后缀：{}", out.content);
        assert!(out.content.contains("a.txt"), "{}", out.content);
    }
}
