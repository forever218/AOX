//! 分层组合清单（阶段 5 的**组合层**）。
//!
//! 读配置从「打开一个文件」变成「合成一个层栈」：
//!
//! ```text
//! profile → bundle（可聚合、可被文件覆盖）→ patch（清单里列出）→ config.toml（永远最后一层）
//! ```
//!
//! **只换来源、不换模型**：合成出来的仍是一棵普通的 TOML 树，交给
//! `toml::from_str::<Config>` 去解析 —— `Config` 的形状一个字段都没动。
//! 这就是设计原则 P1「单一归一化配置树」的兑现方式，也是这条路线能渐进、不返工的原因。
//!
//! 优先级：**越靠后的层越强**，合并规则是「两边都是表就深合，其余整体替换」。
//!
//! 两条要守住的语义：
//!
//! - `config.toml` **永远是最后一层**。所以阶段 1–4 已经落地的键一个都不会失效，
//!   现有配置零迁移成本（决策 A：长期兼容）。
//! - 没有 `<应用根>/aox.toml` 时，层栈退化成 `[config.toml]` 一层 ——
//!   合成结果与阶段 5 之前**逐字段相同**（`no_manifest_is_single_layer` 钉住）。
//!
//! 失败路径一律**启动即报错，不静默跳过**：认不出的 bundle、找不到的 patch、
//! bundle 成环、清单里没有那个 profile —— 都直接失败。静默跳过的后果是
//! 「我明明写了这一层，怎么没生效」，而这类问题最难查。

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::Config;

// ─────────────────────────────── 层 ───────────────────────────────

/// 一层从哪来。决定 dump / `/config` 里怎么标来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    /// 内置 bundle（`include_str!` 进二进制）
    Bundle,
    /// `<应用根>/bundles/<名>.toml` —— 存在就用它，**整层取代**同名内置
    BundleFile,
    /// 清单里列出的 patch 文件（相对应用根）
    Patch,
    /// `--config` 指定的那份。**永远是最后一层。**
    Config,
}

/// 一层：一个名字 + 一个来源类别 + 它贡献的配置树（一张 TOML 表）。
#[derive(Debug, Clone)]
pub struct Layer {
    /// 展示名：bundle 名 / patch 在清单里的写法 / 配置文件名
    pub name: String,
    pub kind: LayerKind,
    /// 补充说明（目前只有「文件覆盖内置」「自定义」两种），拼在标签后面
    pub note: Option<String>,
    /// 这一层贡献的值 —— 组合指令（`bundles`）已经在这一步之前剥掉了
    pub doc: toml::Value,
}

impl Layer {
    /// dump 与 `/config` 里显示的来源标签。
    pub fn label(&self) -> String {
        let head = match self.kind {
            LayerKind::Bundle | LayerKind::BundleFile => format!("bundle:{}", self.name),
            LayerKind::Patch => format!("patch:{}", self.name),
            LayerKind::Config => self.name.clone(),
        };
        match &self.note {
            Some(n) => format!("{head}（{n}）"),
            None => head,
        }
    }

    /// 这一层自己写了多少个值（叶子，不含中间表）。空的聚合 bundle 是 0。
    pub fn value_count(&self) -> usize {
        count_leaves(&self.doc)
    }
}

// ──────────────────────────── 来源表 ────────────────────────────

/// 叶子路径 → 最后写它的那一层的标签。
///
/// 路径用点分（`provider.base_url` / `auth.overrides.fs_*`）。**只记叶子** ——
/// 中间表的「来源」没有意义：一张表常被好几层各写一个键，比如 `[agent]`
/// 在 tools.toml 里定工具超时、在 prompt-core.toml 里定规则文件。
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    map: BTreeMap<String, String>,
}

impl Provenance {
    /// 这个叶子最后被哪一层写过；没写过则 None（= 代码内置默认）。
    pub fn get(&self, path: &str) -> Option<&str> {
        self.map.get(path).map(String::as_str)
    }

    /// 合成树里一共有多少个值。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[allow(dead_code)] // 与 `len` 成对；bin 目标里用不到
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn set(&mut self, path: &str, label: &str) {
        // 空路径 = 整棵树的根，没有意义，不记
        if !path.is_empty() {
            self.map.insert(path.to_string(), label.to_string());
        }
    }
}

// ──────────────────────────── 合成 ────────────────────────────

/// 合成后的结果：层栈 + 合成树 + 来源表。
///
/// 刻意**与 `Config` 解耦**：`compose` 不解析 `Config`，所以哪怕配置本身有毛病
/// （必填项没给、类型写错），`--dump-config` 照样能把合成树打出来 ——
/// 一个「配置坏了就用不了」的诊断工具等于半个废工具。
#[derive(Debug)]
pub struct Composed {
    pub layers: Vec<Layer>,
    pub merged: toml::Value,
    pub provenance: Provenance,
}

impl Composed {
    /// 把合成树解析成 `Config`。形状与阶段 5 之前完全一致。
    pub fn to_config(&self) -> Result<Config> {
        let text = toml::to_string(&self.merged).context("合成后的配置树无法序列化")?;
        let last = self.layers.last().map(Layer::label).unwrap_or_else(|| "（无层）".into());
        toml::from_str::<Config>(&text).with_context(|| {
            format!(
                "合成后的配置解析失败（最后一层是 {last}）—— \
                 多半是必填项没给（如 [provider] 的 base_url / model）或类型写错了；\
                 报错里的行号指的是合成结果、不是你某个文件里的行，\
                 用 `aox --dump-config` 看合成树就能对上是哪个键"
            )
        })
    }

    /// 只取层的来源标签（给 GUI「关于」窗口那一行用）。
    #[allow(dead_code)] // 只有 GUI 会调；bin 目标也会编译本文件
    pub fn layer_labels(&self) -> Vec<String> {
        self.layers.iter().map(Layer::label).collect()
    }

    /// `/config` 的短版面：列了哪几层、各层贡献几个值。
    pub fn layer_summary(&self) -> String {
        let mut s = String::from("配置层（低 → 高，后面的覆盖前面的）：\n");
        for (i, l) in self.layers.iter().enumerate() {
            s.push_str(&format!("  {}. {}（{} 个值）\n", i + 1, l.label(), l.value_count()));
        }
        s.push_str(&format!(
            "共 {} 层，合成后 {} 个值。",
            self.layers.len(),
            self.provenance.len()
        ));
        if self.layers.len() == 1 {
            s.push_str("\n（应用根下没有 aox.toml —— 只用 config.toml 一份，与阶段 5 之前等价。）");
        }
        s
    }

    /// `--dump-config` 的正版面：层清单 + 合成树，每个值后面标来源。
    pub fn dump_text(&self) -> String {
        let mut s = String::new();

        s.push_str("配置层（低 → 高，后面的覆盖前面的；config.toml 永远是最后一层）：\n");
        let n = self.layers.len();
        for (i, l) in self.layers.iter().enumerate() {
            let tail = if i + 1 == n { "   ← 最后一层" } else { "" };
            s.push_str(&format!("  {:>2}  {}{}\n", i + 1, l.label(), tail));
        }

        // (顶层键, 「路径 = 值」的展示, 来源标签)
        let mut rows: Vec<(String, String, String)> = Vec::new();
        collect_rows(&self.merged, "", &mut |path, v| {
            let group = path.split('.').next().unwrap_or_default().to_string();
            let src = self.provenance.get(path).unwrap_or("默认（代码）").to_string();
            rows.push((group, format!("{} = {}", path, inline(v)), src));
        });

        s.push_str(&format!(
            "\n合成后的配置（{} 个值；`#` 后面是最后写这个值的层）：\n",
            rows.len()
        ));
        if rows.is_empty() {
            s.push_str("\n  （空 —— 所有键都留给代码默认。）\n");
        }
        let width = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0).min(72);
        let mut last_group = String::new();
        for (group, display, src) in &rows {
            if *group != last_group {
                s.push_str(&format!("\n  ── {} ──\n", group));
                last_group = group.clone();
            }
            let pad = width.saturating_sub(display.chars().count()).max(1);
            s.push_str(&format!("  {}{}# {}\n", display, " ".repeat(pad), src));
        }

        s.push_str(
            "\n合成树里**没有**的键 = 不参与合成：或没有出厂值（必填项，由你在 config.toml 里给，\n\
             比如 [provider] 的 base_url / model），或由代码在读取时兜底（比如 [theme] 的各尺度默认）。\n",
        );
        s
    }
}

/// 合成 + 解析成 `Config` —— CLI 与 GUI 都走这一条。
pub struct Resolved {
    pub config: Config,
    pub composed: Composed,
}

/// 合成层栈并解析成 `Config`。两个前端（CLI / GUI）唯一的配置入口。
///
/// `manifest`：`--manifest <path>` 的显式覆盖；给 `None` 则看应用根下的 `aox.toml`。
/// `profile`：`--profile <name>` 的临时覆盖；给 `None` 则用清单里 `profile = "…"`，再缺省 `"default"`。
pub fn resolve(
    app_root: &Path,
    config_path: &Path,
    manifest: Option<&Path>,
    profile: Option<&str>,
) -> Result<Resolved> {
    let composed = compose(app_root, config_path, manifest, profile)?;
    let config = composed.to_config()?;
    Ok(Resolved { config, composed })
}

/// 只做合成，不解析 `Config`（诊断入口 `--dump-config` 用）。
pub fn compose(
    app_root: &Path,
    config_path: &Path,
    manifest: Option<&Path>,
    profile: Option<&str>,
) -> Result<Composed> {
    let mut layers: Vec<Layer> = Vec::new();

    // ── 清单：显式给了 `--manifest` 就必须存在；没给则看应用根下的 aox.toml ──
    let explicit = manifest.is_some();
    let manifest_path = match manifest {
        Some(p) => PathBuf::from(p),
        None => app_root.join("aox.toml"),
    };
    if explicit && !manifest_path.is_file() {
        bail!("找不到清单文件 {}", manifest_path.display());
    }

    if manifest_path.is_file() {
        let (default_profile, profiles) = load_manifest(&manifest_path)?;
        let name = profile
            .map(str::to_string)
            .or(default_profile)
            .unwrap_or_else(|| "default".to_string());
        let decl = profiles.get(&name).ok_or_else(|| {
            let mut known: Vec<&str> = profiles.keys().map(String::as_str).collect();
            known.sort_unstable();
            if known.is_empty() {
                anyhow!(
                    "清单文件 {} 里没有 [profiles.{name}]。请补上这一节，\
                     或删掉这个文件（没有它时只用 config.toml 一份，行为与以前一致）。",
                    manifest_path.display()
                )
            } else {
                anyhow!(
                    "清单文件 {} 里没有 profile「{name}」；现有的有：{}",
                    manifest_path.display(),
                    known.join(", ")
                )
            }
        })?;

        layers.append(&mut expand_bundles(app_root, &decl.bundles)?);

        for p in &decl.patches {
            let path = relative_to(app_root, p);
            if !path.is_file() {
                bail!(
                    "patch 文件不存在：{}（在 {} 的 profile「{name}」里列出）",
                    path.display(),
                    manifest_path.display()
                );
            }
            let doc = read_toml(&path, "patch 文件")?;
            layers.push(Layer {
                name: p.clone(),
                kind: LayerKind::Patch,
                note: None,
                doc,
            });
        }
    }

    // ── config.toml：永远是最后一层 ──
    let text = std::fs::read_to_string(config_path).with_context(|| {
        format!(
            "找不到配置文件 {}。请复制 config.example.toml 为 config.toml 并填写 API 配置。",
            config_path.display()
        )
    })?;
    let doc = toml::from_str::<toml::Value>(&text)
        .with_context(|| format!("配置文件 {} 解析失败", config_path.display()))?;
    let name = config_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| config_path.display().to_string());
    layers.push(Layer { name, kind: LayerKind::Config, note: None, doc });

    // ── 合成：后层覆盖前层 ──
    let mut merged = toml::Value::Table(toml::Table::new());
    let mut provenance = Provenance::default();
    for l in &layers {
        let label = l.label();
        merge_at(&mut merged, &l.doc, &label, "", &mut provenance);
    }

    Ok(Composed { layers, merged, provenance })
}

/// 只按名字展开 bundle（不读清单）。`compose` 与单测都走这一条。
///
/// 展开顺序是**后序**：先全部子 bundle，最后才是自己。所以聚合者
/// （`core`）若写了值，能盖住它的子 bundle —— 这也让「在 profile 里后列的 bundle」
/// 稳稳压过先列的（`strict` 就是这么盖住出厂选择的）。
pub fn expand_bundles(app_root: &Path, names: &[String]) -> Result<Vec<Layer>> {
    let mut ex = Expander { app_root, out: Vec::new() };
    let mut stack: Vec<String> = Vec::new();
    for n in names {
        ex.expand(n, &mut stack)?;
    }
    Ok(ex.out)
}

// ──────────────────────────── 展开 ────────────────────────────

struct Expander<'a> {
    app_root: &'a Path,
    out: Vec<Layer>,
}

impl Expander<'_> {
    fn expand(&mut self, name: &str, stack: &mut Vec<String>) -> Result<()> {
        if stack.iter().any(|n| n == name) {
            bail!("bundle 成环：{} → {name}", stack.join(" → "));
        }
        let (text, kind, note, origin) = self.load(name)?;
        let doc = toml::from_str::<toml::Value>(&text)
            .with_context(|| format!("bundle {name}（{origin}）解析失败"))?;
        let toml::Value::Table(mut table) = doc else {
            bail!("bundle {name}（{origin}）的根必须是一张表");
        };

        // `bundles = [...]` 是**组合指令**，不是配置值 —— 先展开，再从值里剥掉。
        let children = match table.get("bundles") {
            Some(toml::Value::Array(a)) => a.clone(),
            Some(_) => bail!("bundle {name}（{origin}）的 bundles 必须是一个字符串数组"),
            None => Vec::new(),
        };
        let child_names: Vec<String> = children
            .iter()
            .map(|c| {
                c.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow!("bundle {name}（{origin}）的 bundles 里有一项不是字符串")
                })
            })
            .collect::<Result<_>>()?;

        stack.push(name.to_string());
        for c in &child_names {
            self.expand(c, stack)?;
        }
        stack.pop();

        table.remove("bundles");

        self.out.push(Layer { name: name.to_string(), kind, note, doc: toml::Value::Table(table) });
        Ok(())
    }

    /// 找一层 bundle 的内容：**应用根下的文件优先**，其次内置。
    ///
    /// 文件存在就**整层取代**内置（不是逐键深合）——「换掉整套出厂选择」才是有用的语义；
    /// 只想改几个键的话，那是 patch 该干的事。
    fn load(&self, name: &str) -> Result<(String, LayerKind, Option<String>, String)> {
        let file = self.app_root.join("bundles").join(format!("{name}.toml"));
        if file.is_file() {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("读不了 bundle 文件 {}", file.display()))?;
            let note = if builtin(name).is_some() {
                Some("文件覆盖内置".to_string())
            } else {
                Some("自定义".to_string())
            };
            let origin = file.display().to_string();
            return Ok((text, LayerKind::BundleFile, note, origin));
        }
        match builtin(name) {
            Some(text) => Ok((text.to_string(), LayerKind::Bundle, None, "内置".to_string())),
            None => bail!("认不出的 bundle「{name}」：既没有内置，也没有 {}", file.display()),
        }
    }
}

/// 内置 bundle 表。`include_str!` 要**字面**路径，所以只能一个个列出来；
/// 加一个 bundle 就要在这里加一行（漏了会在启动时报「认不出的 bundle」，不会静默）。
fn builtin(name: &str) -> Option<&'static str> {
    Some(match name {
        "core" => include_str!("../../bundles/core.toml"),
        "llm-openai" => include_str!("../../bundles/llm-openai.toml"),
        "tools" => include_str!("../../bundles/tools.toml"),
        "store-jsonl" => include_str!("../../bundles/store-jsonl.toml"),
        "loop-standard" => include_str!("../../bundles/loop-standard.toml"),
        "prompt-core" => include_str!("../../bundles/prompt-core.toml"),
        "strict" => include_str!("../../bundles/strict.toml"),
        _ => return None,
    })
}

// ──────────────────────────── 合并 ────────────────────────────

/// 把 `src` 合进 `dst`：**两边都是表就深合，其余整体替换**（后层覆盖前层）。
///
/// `path` 是当前节点在配置树里的点分路径 —— 顺手把每个叶子最后被谁写记进 `prov`。
fn merge_at(dst: &mut toml::Value, src: &toml::Value, label: &str, path: &str, prov: &mut Provenance) {
    if let (toml::Value::Table(dt), toml::Value::Table(st)) = (&mut *dst, src) {
        for (k, v) in st {
            let child = join_path(path, k);
            match dt.get_mut(k) {
                // 已有的键：继续往下合（是表就深合，不是就整块换掉）
                Some(slot) => merge_at(slot, v, label, &child, prov),
                // 新键：整棵搬进来，并把它的叶子全记成本层所写
                None => {
                    dt.insert(k.clone(), v.clone());
                    record(v, label, &child, prov);
                }
            }
        }
    } else {
        *dst = src.clone();
        record(src, label, path, prov);
    }
}

/// 记下 `v` 的**叶子**。数组是「整体替换」的，所以只记到数组这一层为止。
fn record(v: &toml::Value, label: &str, path: &str, prov: &mut Provenance) {
    match v {
        toml::Value::Table(t) => {
            for (k, c) in t {
                record(c, label, &join_path(path, k), prov);
            }
        }
        _ => prov.set(path, label),
    }
}

fn count_leaves(v: &toml::Value) -> usize {
    match v {
        toml::Value::Table(t) => t.values().map(count_leaves).sum(),
        _ => 1,
    }
}

/// 把合成树摊平成一串「叶子路径 → 值」。
fn collect_rows(v: &toml::Value, path: &str, out: &mut dyn FnMut(&str, &toml::Value)) {
    match v {
        // 空表也当一个叶子（不然 `[foo]` 这种空表会在 dump 里凭空消失）
        toml::Value::Table(t) if !t.is_empty() => {
            for (k, c) in t {
                collect_rows(c, &join_path(path, k), out);
            }
        }
        _ => out(path, v),
    }
}

fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// 一行的内联写法（表 / 数组都压成一行，方便对齐与 grep）。
fn inline(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => format!("{s:?}"),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(inline).collect();
            format!("[{}]", items.join(", "))
        }
        toml::Value::Table(t) => {
            let items: Vec<String> =
                t.iter().map(|(k, val)| format!("{k} = {}", inline(val))).collect();
            format!("{{ {} }}", items.join(", "))
        }
    }
}

// ──────────────────────────── 清单 ────────────────────────────

/// `aox.toml` 里一个 profile。
struct ProfileDecl {
    bundles: Vec<String>,
    patches: Vec<String>,
}

/// 读清单：`profile = "…"`（默认用哪个）+ 每个 `[profiles.<名>]`。
fn load_manifest(path: &Path) -> Result<(Option<String>, BTreeMap<String, ProfileDecl>)> {
    let doc = read_toml(path, "清单文件")?;
    let root = doc
        .as_table()
        .ok_or_else(|| anyhow!("清单文件 {} 的根必须是一张表", path.display()))?;

    let default_profile = root.get("profile").and_then(|v| v.as_str()).map(str::to_string);

    let mut profiles = BTreeMap::new();
    if let Some(t) = root.get("profiles").and_then(|v| v.as_table()) {
        for (name, v) in t {
            let pt =
                v.as_table().ok_or_else(|| anyhow!("[profiles.{name}] 必须是一张表"))?;
            profiles.insert(
                name.clone(),
                ProfileDecl {
                    bundles: str_list(pt.get("bundles"), &format!("[profiles.{name}].bundles"))?,
                    patches: str_list(pt.get("patches"), &format!("[profiles.{name}].patches"))?,
                },
            );
        }
    }
    Ok((default_profile, profiles))
}

fn str_list(v: Option<&toml::Value>, what: &str) -> Result<Vec<String>> {
    match v {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("{what} 里有一项不是字符串"))
            })
            .collect(),
        Some(_) => bail!("{what} 必须是一个字符串数组"),
    }
}

fn read_toml(path: &Path, what: &str) -> Result<toml::Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读不了{what} {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{what} {} 解析失败", path.display()))
}

/// patch / bundle 路径：相对路径按应用根算，绝对路径原样采用。
fn relative_to(app_root: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        app_root.join(path)
    }
}

// ──────────────────────────── 单测 ────────────────────────────
//
// 这些测试全都自建「隔离根」：一个独立目录 + 自己写的 config.toml / aox.toml /
// patch / bundles/*.toml。不碰仓库里真实的 config.toml，也就不受开发者本机配置影响。

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例一个独立目录，建在系统临时目录下。
    ///
    /// 全项目唯一的删除动作就在这里，范围被三条同时限定：必须是**我们自己命名**的目录、
    /// 必须落在系统临时目录下、名字必须带 `aox-layer-test-` 前缀。
    /// （项目里那条「不做目录级删除」的约定，防的正是范围失控的递归删。）
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aox-layer-test-{tag}"));
        assert!(dir.starts_with(std::env::temp_dir()), "拒绝在临时目录之外清理");
        assert!(
            dir.file_name().unwrap().to_string_lossy().starts_with("aox-layer-test-"),
            "只清理自己造的目录"
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, text).unwrap();
    }

    /// 只填必填项的一份配置。用 `format!("{:#?}")` 对拍是为了**逐字段**比 ——
    /// `Config` 上没挂 `PartialEq`（路线明确说不动 `Config` 本体），
    /// 而它的每个字段都实现了 `Debug`，Debug 串相等 ⇔ 字段相等。
    const MINIMAL: &str = r#"
[provider]
base_url = "https://example.com/v1"
model = "m"
api_key = "k"
"#;

    fn core_manifest(dir: &Path) {
        write(
            dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\n",
        );
    }

    /// 没有 `aox.toml` 时，层栈就是 `[config.toml]` 一层 ——
    /// 合成结果与「直接解析 config.toml」逐字段相同（阶段 5 的零漂移保证）。
    #[test]
    fn no_manifest_is_single_layer() {
        let dir = tmpdir("single");
        write(&dir, "config.toml", MINIMAL);

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        assert_eq!(c.layers.len(), 1, "没有 aox.toml 就该只有 config.toml 一层");
        assert_eq!(c.layers[0].kind, LayerKind::Config);
        assert_eq!(c.layers[0].label(), "config.toml");
        assert_eq!(c.provenance.get("provider.base_url"), Some("config.toml"));

        let direct: Config = toml::from_str(MINIMAL).unwrap();
        let composed = c.to_config().unwrap();
        assert_eq!(
            format!("{direct:#?}"),
            format!("{composed:#?}"),
            "合成结果必须与直接解析 config.toml 逐字段相同"
        );
    }

    /// **决策 B 的代价由这条测试兜住**：叶子 bundle 里写的出厂值，必须与代码默认
    /// （`#[serde(default)]` 那一份）逐字段相同。任何一边改了没同步，这里就红。
    #[test]
    fn core_equals_code_defaults() {
        let dir = tmpdir("core-defaults");
        write(&dir, "config.toml", MINIMAL);
        core_manifest(&dir);

        let direct: Config = toml::from_str(MINIMAL).unwrap();
        let composed =
            compose(&dir, &dir.join("config.toml"), None, None).unwrap().to_config().unwrap();

        assert_eq!(
            format!("{direct:#?}"),
            format!("{composed:#?}"),
            "core 的出厂值必须与代码默认逐字段相同（bundle 与 serde 默认不能悄悄漂移）"
        );
    }

    /// 后层覆盖前层：patch 盖 bundle、config.toml 盖 patch；而且是**表深合**。
    #[test]
    fn patch_overrides_bundle_and_config_wins_last() {
        let dir = tmpdir("order");
        write(&dir, "config.toml", MINIMAL);
        write(
            &dir,
            "aox.local.toml",
            "[provider]\nstream = false\n\n[agent]\ntool_timeout_secs = 5\n",
        );
        write(
            &dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\npatches = [\"aox.local.toml\"]\n",
        );

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        let cfg = c.to_config().unwrap();

        assert!(!cfg.provider.stream, "patch 的 stream = false 应压过 bundle 的 true");
        assert_eq!(cfg.agent.tool_timeout_secs, 5, "patch 应压过 bundle 的 60");
        assert_eq!(
            cfg.agent.max_result_chars, 20000,
            "agent 表是深合的：patch 只改了超时，不该把 max_result_chars 一起抹掉"
        );
        assert_eq!(cfg.provider.model, "m", "config.toml 仍是最强的一层");

        assert_eq!(c.provenance.get("provider.stream"), Some("patch:aox.local.toml"));
        assert_eq!(c.provenance.get("agent.tool_timeout_secs"), Some("patch:aox.local.toml"));
        assert_eq!(c.provenance.get("agent.max_result_chars"), Some("bundle:tools"));
        assert_eq!(c.provenance.get("auth.meta_tools"), Some("bundle:tools"));
    }

    /// `config.toml` 永远最后一层：连 patch 也压不住它。
    #[test]
    fn config_toml_beats_every_layer() {
        let dir = tmpdir("config-last");
        write(&dir, "aox.local.toml", "[store]\nbackend = \"nope\"\n");
        write(
            &dir,
            "config.toml",
            "[provider]\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\napi_key = \"k\"\n\n[store]\nbackend = \"jsonl\"\n",
        );
        write(
            &dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\npatches = [\"aox.local.toml\"]\n",
        );

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        assert_eq!(c.layers.last().unwrap().kind, LayerKind::Config);
        assert_eq!(c.provenance.get("store.backend"), Some("config.toml"));
    }

    /// `--profile <名>` 与清单里的 `profile = "…"` 都能选中 profile。
    #[test]
    fn profile_selection_and_default() {
        let dir = tmpdir("profiles");
        write(&dir, "config.toml", MINIMAL);
        write(
            &dir,
            "aox.toml",
            "profile = \"strict\"\n\n[profiles.default]\nbundles = [\"core\"]\n\n[profiles.strict]\nbundles = [\"core\", \"strict\"]\n",
        );

        // 清单说了 profile = "strict"，不带参数就走它
        let s = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        assert!(s.layer_labels().iter().any(|l| l == "bundle:strict"), "应走 strict");
        assert_eq!(
            s.provenance.get("auth.meta_tools"),
            Some("bundle:strict"),
            "strict 排在后、盖住 tools"
        );

        // 显式换回 default
        let d = compose(&dir, &dir.join("config.toml"), None, Some("default")).unwrap();
        assert!(!d.layer_labels().iter().any(|l| l == "bundle:strict"), "default 不含 strict");
        assert_eq!(d.provenance.get("auth.meta_tools"), Some("bundle:tools"));
    }

    /// 应用根下的 `bundles/<名>.toml` 存在时**整层取代**内置，并在来源里标出来。
    #[test]
    fn file_bundle_overrides_builtin() {
        let dir = tmpdir("file-override");
        write(&dir, "config.toml", MINIMAL);
        write(&dir, "bundles/tools.toml", "[auth]\nmeta_tools = \"deny\"\n");
        core_manifest(&dir);

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        let labels = c.layer_labels();
        assert!(
            labels.iter().any(|l| l == "bundle:tools（文件覆盖内置）"),
            "该标出这是文件覆盖：{labels:?}"
        );

        let cfg = c.to_config().unwrap();
        assert_eq!(cfg.auth.meta_tools, "deny", "文件里的值生效");
        assert_eq!(
            cfg.auth.plugin_tools, "auto",
            "文件是整层取代内置：它没写的 plugin_tools 回落到代码默认（tools.toml 那一行已经不在层里）"
        );
    }

    /// 数组是**整体替换**，不是追加 —— 这也是「明确不做追加语义」的钉子。
    #[test]
    fn arrays_are_replaced_whole_not_appended() {
        let dir = tmpdir("arrays");
        write(&dir, "config.toml", MINIMAL);
        write(&dir, "p1.toml", "[prompt]\norder = [\"env\"]\n");
        write(&dir, "p2.toml", "[prompt]\norder = [\"rules\"]\n");
        write(
            &dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\npatches = [\"p1.toml\", \"p2.toml\"]\n",
        );

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        let cfg = c.to_config().unwrap();
        assert_eq!(cfg.prompt.order, vec!["rules".to_string()], "后一层整个换掉数组，不是拼接");
        assert_eq!(c.provenance.get("prompt.order"), Some("patch:p2.toml"));
    }

    /// 带 `*` 的 override 键必须加引号才合法（`*` 不在 TOML 裸键字符集里）。
    /// 这条测试同时验证「合成要走一遍序列化 → 再解析」不会把这样的键弄坏。
    #[test]
    fn wildcard_override_key_survives_roundtrip() {
        assert!(
            toml::from_str::<Config>(
                "[provider]\nbase_url = \"u\"\nmodel = \"m\"\n\n[auth.overrides]\nfs_* = \"deny\"\n"
            )
            .is_err(),
            "裸 fs_* 不是合法 TOML 键 —— 这正是 config.example.toml 里要写引号的原因"
        );

        let dir = tmpdir("wildcard");
        write(
            &dir,
            "config.toml",
            "[provider]\nbase_url = \"https://example.com/v1\"\nmodel = \"m\"\napi_key = \"k\"\n\n[auth.overrides]\n\"fs_*\" = \"deny\"\n",
        );

        let c = compose(&dir, &dir.join("config.toml"), None, None).unwrap();
        let cfg = c.to_config().unwrap();
        assert_eq!(cfg.auth.overrides.get("fs_*").map(String::as_str), Some("deny"));
        assert_eq!(c.provenance.get("auth.overrides.fs_*"), Some("config.toml"));
    }

    /// `--dump-config` 的正文必须**把每个值的来源标出来**（验收标准 1）。
    #[test]
    fn dump_marks_each_value_with_its_layer() {
        let dir = tmpdir("dump");
        write(&dir, "config.toml", MINIMAL);
        core_manifest(&dir);

        let d = compose(&dir, &dir.join("config.toml"), None, None).unwrap().dump_text();
        assert!(d.contains("bundle:store-jsonl"), "{d}");
        assert!(d.contains("# bundle:store-jsonl"), "每个值后面要有来源：{d}");
        assert!(d.contains("store.backend = \"jsonl\""), "{d}");
        assert!(d.contains("provider.base_url = \"https://example.com/v1\""), "{d}");
        assert!(d.contains("# config.toml"), "{d}");
        assert!(d.contains("← 最后一层"), "要标出哪一层是最后一层：{d}");
    }

    /// `/config` 的短版面：层的数量、各层贡献几个值。
    #[test]
    fn layer_summary_lists_layers_and_counts() {
        let dir = tmpdir("summary");
        write(&dir, "config.toml", MINIMAL);
        core_manifest(&dir);

        let s = compose(&dir, &dir.join("config.toml"), None, None).unwrap().layer_summary();
        assert!(s.contains("bundle:loop-standard（0 个值）"), "空的 bundle 要显示 0：{s}");
        assert!(s.contains("bundle:core（0 个值）"), "聚合者自己不带值：{s}");
        assert!(s.contains("共 7 层"), "五个叶子 + core + config.toml：{s}");
        assert!(!s.contains("没有 aox.toml"), "有 aox.toml 时不该出现那句：{s}");

        let one = compose(&dir, &dir.join("config.toml"), Some(&dir.join("nope.toml")), None);
        assert!(one.is_err(), "显式指定不存在的清单必须报错");
    }

    /// 认不出的 bundle / 找不到的 patch / bundle 成环 / 没有那个 profile：
    /// 一律报错，不静默跳过。
    #[test]
    fn failure_paths_are_loud() {
        let dir = tmpdir("failures");
        write(&dir, "config.toml", MINIMAL);

        // ① 认不出的 bundle
        write(&dir, "aox.toml", "profile = \"default\"\n\n[profiles.default]\nbundles = [\"nope\"]\n");
        let e = compose(&dir, &dir.join("config.toml"), None, None).unwrap_err();
        assert!(format!("{e:#}").contains("认不出的 bundle"), "{e:#}");

        // ② 找不到的 patch
        write(
            &dir,
            "aox.toml",
            "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\npatches = [\"nope.toml\"]\n",
        );
        let e = compose(&dir, &dir.join("config.toml"), None, None).unwrap_err();
        assert!(format!("{e:#}").contains("patch 文件不存在"), "{e:#}");

        // ③ bundle 成环
        write(&dir, "bundles/loop-a.toml", "bundles = [\"loop-b\"]\n");
        write(&dir, "bundles/loop-b.toml", "bundles = [\"loop-a\"]\n");
        write(&dir, "aox.toml", "profile = \"default\"\n\n[profiles.default]\nbundles = [\"loop-a\"]\n");
        let e = compose(&dir, &dir.join("config.toml"), None, None).unwrap_err();
        assert!(format!("{e:#}").contains("成环"), "{e:#}");

        // ④ 清单里没有那个 profile
        write(&dir, "aox.toml", "profile = \"default\"\n\n[profiles.default]\nbundles = [\"core\"]\n");
        let e = compose(&dir, &dir.join("config.toml"), None, Some("nope")).unwrap_err();
        assert!(format!("{e:#}").contains("没有 profile"), "{e:#}");

        // ⑤ 显式指定的清单不存在
        let e = compose(&dir, &dir.join("config.toml"), Some(Path::new("不存在的清单.toml")), None)
            .unwrap_err();
        assert!(format!("{e:#}").contains("找不到清单文件"), "{e:#}");

        // ⑥ 配置文件本身缺失
        let e = compose(&dir, &dir.join("nope-config.toml"), None, None).unwrap_err();
        assert!(format!("{e:#}").contains("找不到配置文件"), "{e:#}");
    }

    /// 随包交付的两份示例（`config.example.toml` / `aox.example.toml`）必须真的能用。
    ///
    /// 这条测试是冲着「示例写得挺好看、用户照抄却启动不了」来的 ——
    /// 阶段 2 的主题示例就踩过一次（示例用 `[theme]` 嵌套，而加载器当时只认裸主题）。
    /// 测试在仓库根下跑，此时 `bundles/*.toml` 这些**文件**会盖过内置副本，
    /// 所以来源标签断言只认名字、不认后缀。
    #[test]
    fn shipped_examples_compose_and_parse() {
        let root = Path::new(".");
        let cfg_path = root.join("config.example.toml");
        let manifest = root.join("aox.example.toml");

        // ① 示例配置本身要能解析
        let base: Config =
            toml::from_str(&std::fs::read_to_string(&cfg_path).expect("示例配置要能读")).unwrap();

        // ② 示例清单要能跟它合成，且合成结果要能解析回 Config
        let composed = compose(root, &cfg_path, Some(&manifest), None).expect("示例清单应能合成");
        let cfg = composed.to_config().expect("示例合成结果应能解析");
        assert_eq!(cfg.provider.model, base.provider.model);
        assert_eq!(cfg.provider.base_url, base.provider.base_url);
        assert_eq!(cfg.provider.max_tokens, base.provider.max_tokens);
        assert_eq!(composed.layers.last().unwrap().kind, LayerKind::Config);

        // 出厂选择里的值确实进了合成树，config.toml 又在最上面。
        // 挑 `prompt.order` 是因为示例配置**没写** [prompt]（整段是注释），
        // 所以它只能来自 bundle —— 用它验证「bundle 真的参与了合成」。
        let prompt_order = composed.provenance.get("prompt.order").unwrap_or("");
        assert!(
            prompt_order.contains("prompt-core"),
            "示例配置没写 [prompt]，该由 bundle 提供；实际是 {prompt_order}"
        );
        assert_eq!(
            composed.provenance.get("agent.tool_timeout_secs"),
            Some("config.example.toml"),
            "示例配置写的 120 秒必须赢过出厂 bundle"
        );

        // ③ 示例清单里的第二个 profile 也要能合成。
        //    注意这里**不能**断言 strict 的值赢了 —— 示例配置自己写了 [auth]，
        //    而 config.toml 永远是最强的一层，所以 strict 那几个键注定被它盖住。
        //    能验证的是「strict 这一层确实进了栈」。
        let strict = compose(root, &cfg_path, Some(&manifest), Some("strict")).unwrap();
        assert!(
            strict.layer_labels().iter().any(|l| l.contains("strict")),
            "层栈里该有 strict：{:?}",
            strict.layer_labels()
        );
        assert_eq!(
            strict.layers.len(),
            composed.layers.len() + 1,
            "strict profile 比 default 正好多一层"
        );
    }
}
