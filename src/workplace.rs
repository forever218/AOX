//! 工作区：**模型的文件能力能被限制在哪**。
//!
//! 这是沙箱方案 ③「把文件能力收回宿主」的宿主侧。它回答一个问题：
//! 「这个路径，当前档位允许碰吗？」
//!
//! # 三档
//!
//! | 档位 | 含义 | 判定 |
//! |------|------|------|
//! | `default`（**配置默认**） | 每段新对话在 `<应用根>/workplace/<YYYYMMDDHHMMSS>/` 开一块地 | 写·删只准这块地；**读不设限** |
//! | `full` | 完全访问，与加工作区之前逐字节一致 | 一切放行 |
//! | `fixed`（`root = "..."`） | 固定一块地（GUI 的「选择工作区」等价于它） | 同 `default` |
//!
//! **配置默认是 `default`（受限）**，2026-10-05 由 `full` 改过来：用户要的就是
//! 「每次启动**默认受限**」。于是「不写 `[workplace]` = 行为与加这一层之前一致」这条
//! 不再成立 —— 不写就是受限档，想要不受限得显式写 `mode = "full"`。
//! 另外 `/new` 一律回到 `default` 档（见 [`Workplace::reset_for_new_conversation`]）。
//!
//! # 读写不对称（本模块最重要的一条）
//!
//! 受限档位下**只挡落盘，不挡读**：
//!
//! - `read_file` / `list_dir`（[`Access::Read`]）：整个设备都能读，**不受工作区限制**，
//!   连「地还没开」（[`Active::Pending`]）时也照读不误。
//! - `write_file` / `remove`（[`Access::Write`]）：必须落在工作区内，相对路径一律拒。
//!
//! 理由是后果不对称：读是**取信息**，写删是**改设备**。想读一份散落在别处的资料
//! （日志、配置、别处的项目）是很常见的正当需求，为它设一道提权流程只会把用户逼去
//! 常开「完全访问」—— 边界反而更松；而「只能在划定的那块地里落笔」才是这个功能
//! 真正要保住的东西。细节点见 [`Access`]。
//!
//! # 两个必须讲清楚的语义
//!
//! **① 只看绝对路径。** 受限档位下**相对路径一律拒绝**。原因是宿主与插件子进程
//! 的工作目录不是同一个（插件的 cwd 仍是应用根，见 `plugin::spawn_command`），
//! 同一串 `config.toml` 在两边会指到两个不同文件 —— 那种「宿主以为在里面、
//! 插件实际去了外面」的不一致，比多写几个字符危险得多。拒绝文案里会把根直接告诉模型。
//!
//! **② 越界不是静默拦下。** 见 [`Reject::model_text`]：那是一份**给模型看的契约**，
//! 三段是硬要求 —— 发生了什么 / 这是权限边界不是工具坏了、别重试 / 请转述用户并
//! 告诉他怎么提权。少了任何一段，模型就会一路重试同一个越界调用，白烧轮次。
//!
//! # 边界在哪
//!
//! 这一层管的是「**宿主提供的能力**」：宿主内置的文件工具，以及插件工具声明的路径参数
//! （宿主只能按参数名猜 —— 见 [`scan_paths`]，那是张粗网）。
//! **它管不了一个不守规矩的插件自己 `open()` 文件** —— 那要 OS 级的 AppContainer，
//! 不在本模块的能力范围内。别把它当沙箱用。

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::config::WorkplaceConfig;

/// 受限档位下的工作区子目录名（`<应用根>/workplace/`）。
pub const WORKPLACE_DIR: &str = "workplace";

/// 宿主内置文件工具的**参数**里，表示「路径」的那个键。
///
/// 单独抽出来是因为判定要精确读它：这些工具的 schema 是宿主自己定义的，
/// 所以不需要猜（与插件工具那边的 [`scan_paths`] 形成对照）。
pub const PATH_ARG: &str = "path";

/// 参数名命中这些词时，值被当作路径（只用于**插件工具**的粗网扫描）。
///
/// 刻意保守：漏判只是「少一层粗网」，而误判会直接拒掉一次合法调用，那更烦人。
///
/// 这里**没有**给插件注入「工作区根」的机制（比如一个 `AOX_WORKSPACE_ROOT` 环境变量）：
/// `default` 档位的根是**首条消息时**才建的，而插件进程在启动 / 被创建的那一刻就已经
/// 拉起来了 —— 那一刻还没有值可注，注进去只能是个过期或空的东西。所以插件要守边界，
/// 正确做法是**声明路径参数**，让宿主这层粗网来判（它判不准也只多拒一次，不会漏放行）。
const PATH_KEYS: &[&str] = &[
    "path", "paths", "file", "files", "filename", "filepath", "file_path", "dir", "dirs", "directory",
    "folder", "root", "src", "dst", "dest", "destination", "target", "cwd", "workdir", "working_dir",
    "save_to", "input_path", "output_path",
];

/// 运行期档位。GUI / CLI 可以切，切完立刻生效（下一次工具调用就按新档位判）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// 每段新对话开一块新地。
    Default,
    /// 固定一块地。
    Fixed(PathBuf),
    /// 不受限。
    Full,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Default => "默认",
            Mode::Fixed(_) => "选择工作区",
            Mode::Full => "完全访问",
        }
    }
}

/// 一次路径访问的**意图** —— 决定受限档位下工作区边界管不管它。
///
/// 语义（2026-10-04 定为）：**读不设限，写和删限定在工作区里**。
///
/// 为什么不把「读」也一起管住：两者对设备造成的后果不对称 ——
/// 读是**取信息**，读错了顶多是看到了不该看的内容；写、删是**改设备**，
/// 一次越界就可能毁掉工作区之外的东西（而且是不可逆的）。想读一份散落在
/// 别处的资料（日志、配置、别处的项目）是很常见的正当需求，为它开一道
/// 提权流程只会逼着用户一直开「完全访问」，反而让边界形同虚设；
/// 而「AOX 只能在划定的那块地里落笔」才是这个功能真正要保住的东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 只读（`read_file` / `list_dir`）：**整个设备都能读**，不受工作区限制。
    Read,
    /// 落盘（`write_file` / `remove`）：**必须落在工作区内**（`full` 档除外）。
    Write,
}

/// 当前生效状态。
///
/// 带 `Clone` 是为了让 [`Workplace::report`] 能「先取快照、再释放锁」地格式化 ——
/// 见那里的注释（持锁时再调会重入锁的方法 = 死锁）。
#[derive(Debug, Clone)]
enum Active {
    /// 不受限（配置就是 `full`）。
    Full,
    /// 受限，但**根还没定**：`default` 档位、本对话还没发过消息。
    ///
    /// 单独留一个状态而不是拿 `Option` 把「不受限」和「还没定」混在一起：
    /// 这两者的判定**相反**（前者全放行、后者全拒绝）。混成一个 `None` 就会
    /// 出现「受限档位下因为还没建目录而静默全放行」——那是最坏的一种失效。
    Pending,
    /// 受限，根已定。
    At(PathBuf),
}

/// 一段对话的**工作区存档**（`.jsonl` 里 `kind:"workplace"` 那一行）。
///
/// 三个字段各回答一个问题：
/// - `mode` / `root`：**当前**档位与它绑的地（`fixed` 是点名那块；`default` 是自建那块）；
/// - `own`：本对话「默认」档自己的那块地 —— **即使在 `full` / `fixed` 档下也要留着**，
///   否则「切走再切回默认」只能重新建一块，工作区就会无限累加。
///
/// 为什么存进对话文件而不是另开一个旁路文件：它属于这段对话。对话被删、被裁剪、
/// 被换后端时，这条记录自然跟着走，不会留下孤儿。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    /// `"default"` / `"fixed"` / `"full"`。空串 = 认不出来（调用方应当当没有记录）。
    pub mode: String,
    /// 当前档位绑的根；`full` 没有根。
    pub root: Option<PathBuf>,
    /// 本对话「默认」档自建的那块地（还没建就是 `None`）。
    pub own: Option<PathBuf>,
}

impl Record {
    /// 写成 `.jsonl` 的一行（`kind` 由存储层补，见 `store::jsonl`）。
    ///
    /// 与用量记录同一条规矩：**没有的字段不写**，不用空串或 `null` 去冒充。
    pub fn to_value(&self) -> serde_json::Value {
        let mut v = serde_json::json!({ "mode": self.mode });
        let mut put = |key: &str, p: &Option<PathBuf>| {
            if let Some(p) = p {
                v[key] = serde_json::json!(p.display().to_string());
            }
        };
        put("root", &self.root);
        put("own", &self.own);
        v
    }

    /// 从一行读回来；**认不出的行返回 `None`**（缺 `mode`、或值不是字符串）。
    pub fn from_value(v: &serde_json::Value) -> Option<Self> {
        let mode = v.get("mode").and_then(|m| m.as_str())?.trim().to_lowercase();
        if mode.is_empty() {
            return None;
        }
        let path = |key: &str| {
            v.get(key)
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        };
        Some(Self { mode, root: path("root"), own: path("own") })
    }
}

/// 运行期的档位与根。跟着 `Runtime` 走，CLI / GUI / selftest 共用同一份实现。
///
/// **不存配置**（曾经存过一份 `cfg`）：档位的判据是状态里的 `picked`，不是配置 ——
/// 配置只用来定**启动那一刻**的档（见 `new`），此后一切以运行期状态为准。
/// 留着那份配置只会诱使人再去「按配置推档位」，而那是踩过的坑（见 `State::picked`）。
pub struct Workplace {
    /// `std::sync::Mutex`：只在同步上下文读写，**从不跨 `await` 持锁**
    /// （判定会在工具调用路径上跑，那个 future 必须保持 `Send`）。
    state: std::sync::Mutex<State>,
}

#[derive(Debug)]
struct State {
    active: Active,
    /// 本对话是否已经「开始」（= 已经建过/选定过根）。
    /// `default` 档位靠它保证「一段对话只建一次目录」，而不是每轮都建一个。
    started: bool,
    /// **本对话「默认」档自己的那块地**（第一次发消息时建的）；`None` = 还没建。
    ///
    /// 必须与 `active` 分开记：`active` 会被切档覆盖。切到「完全访问」再切回「默认」时，
    /// 光看 `active` 已经不知道原来那块地是哪一块，只能重新建 —— 结果是每切一个来回就多
    /// 一个空目录，攒出一堆 `workplace/<时间戳>/`（用户 2026-10-05 报的）。
    /// 有了它，「只要档位是默认，就还是本对话第一次建的那一块」才成立。
    ///
    /// 新建对话 / 打开另一段对话时清掉（新对话本来就该另开一块地）。
    own: Option<PathBuf>,
    /// `active` 里的根是**用户点名挑的**（`fixed`，或运行期切档），还是 `default` 档现建的？
    ///
    /// 这条只能记在状态里，**不能拿 `cfg.mode` 推**：配置写 `default` 的用户照样能在运行期
    /// 挑一块固定的地（`/workplace <目录>`、GUI 的选择器），那时档位是「选择工作区」而不是
    /// 「默认」。以前拿配置推，于是同一个动作被说成两种档位：`set_mode` 报「已切到
    /// 「选择工作区」：X」，紧接着 `snapshot()` 报「档位：默认」—— GUI 上表现为按钮写着
    /// 「默认」、打勾也在「默认」上，而写入实际去了 X。（实机撞见的，2026-10-04 修。）
    picked: bool,
}

impl Workplace {
    /// 从配置构造。**认不出的 `mode` 在启动时直接报错**，不静默退回 `full` ——
    /// 与 `[store] backend` / `[loop] impl` 同一条原则：写了却不生效是最难查的一类问题。
    pub fn new(cfg: WorkplaceConfig, app_root: &Path) -> anyhow::Result<Self> {
        let mode = cfg.mode.trim().to_ascii_lowercase();
        let active = match mode.as_str() {
            "full" => Active::Full,
            "default" => Active::Pending,
            "fixed" => {
                let raw = cfg.root.as_deref().unwrap_or("").trim();
                if raw.is_empty() {
                    anyhow::bail!(
                        "[workplace] mode = \"fixed\" 必须同时给 root = \"<目录>\"\
                         （想要「每段对话一块新地」请用 mode = \"default\"）"
                    );
                }
                let p = PathBuf::from(raw);
                let p = if p.is_absolute() { p } else { app_root.join(p) };
                // 固定的工作区**现在就建**：用户在配置里点名了一块地，就该有那块地。
                // `default` 档位不同 —— 那个要等首条消息（否则点了按钮没说话也会留空目录）。
                let p = normalize(&p);
                std::fs::create_dir_all(&p)
                    .map_err(|e| anyhow::anyhow!("[workplace] root = {} 无法创建: {e}", p.display()))?;
                Active::At(p)
            }
            other => anyhow::bail!(
                "[workplace] mode = {other:?} 认不出来。可用值：\"default\"（每段对话一块新地，默认）、\
                 \"full\"（不受限）、\"fixed\"（配合 root 指定固定目录）"
            ),
        };
        Ok(Self {
            state: std::sync::Mutex::new(State {
                active,
                started: matches!(mode.as_str(), "full" | "fixed"),
                // 启动时还没有「本对话自建的地」—— 连这段对话本身都还没开始。
                own: None,
                // 只有 `fixed` 档是「用户点名挑的」；`full` 没有根、`default` 的地是现建的。
                picked: mode == "fixed",
            }),
        })
    }

    /// 应用根（建 `workplace/` 用）。
    fn workdir_of(app_root: &Path) -> PathBuf {
        app_root.join(WORKPLACE_DIR)
    }

    /// 当前生效的根；`None` = 不受限。
    pub fn active_root(&self) -> Option<PathBuf> {
        match &self.lock().active {
            Active::At(p) => Some(p.clone()),
            _ => None,
        }
    }

    /// 界面用的快照：**一次取锁**同时拿到「档位」与「当前生效的根」。
    ///
    /// 为什么不让调用方分别调 [`Workplace::mode`] 与 [`Workplace::active_root`]：
    /// 那要取两次锁，而且两次之间状态可能已经被改掉（最典型的是首条消息刚好在建地），
    /// 于是界面上的档位与目录对不上。**档位与根的映射只在这里写一遍** —— `mode()`
    /// 也走它，免得将来两边改岔：`/workplace` 说「默认」而界面在「选择工作区」上打勾。
    ///
    /// ⚠️ **不要在持锁的上下文里调它** —— 它自己也要锁 `state`，而 `std::sync::Mutex`
    /// 不可重入，嵌套调用会**整进程死锁**（[`Workplace::report`] 上一版就是这么挂的）。
    /// 要么在取锁之前调，要么先取到快照。
    pub fn snapshot(&self) -> (Mode, Option<PathBuf>) {
        Self::snapshot_of(&self.lock())
    }

    /// 「档位 + 根」的**唯一**映射。`snapshot` 与 `persisted` 都读它 ——
    /// 免得将来两边改岔（一个说「默认」、另一个存成 `fixed`）。
    fn snapshot_of(st: &State) -> (Mode, Option<PathBuf>) {
        let mode = match &st.active {
            Active::Full => Mode::Full,
            Active::Pending => Mode::Default,
            // ⚠️ 判据是**状态里的 `picked`**，不是配置里的 `cfg.mode`。配置写 `default` 的
            // 用户照样能在运行期挑一块固定的地（`/workplace <目录>`、GUI 的选择器），
            // 那时档位就是「选择工作区」；拿配置推会把同一个动作说成两种档位。
            // 反过来 `default` 档现建的地报「默认」而不是「选择工作区」—— 它只是当时
            // 现建了一块，不是用户点名的那一块。
            Active::At(_) if !st.picked => Mode::Default,
            Active::At(p) => Mode::Fixed(p.clone()),
        };
        let root = match &st.active {
            Active::At(p) => Some(p.clone()),
            _ => None,
        };
        (mode, root)
    }

    /// 当前档位。等价于 `self.snapshot().0`。
    pub fn mode(&self) -> Mode {
        self.snapshot().0
    }

    /// 把当前状态压成**可存档**的形态（见 [`Record`]）。
    ///
    /// **一次取锁**把三样一起取到：分两次取会出现「档位是切档之后的、`own` 是切档之前的」
    /// 这种半截快照，存下去就再也还原不回来了。
    pub fn persisted(&self) -> Record {
        let st = self.lock();
        let (mode, root) = Self::snapshot_of(&st);
        let mode = match mode {
            Mode::Default => "default",
            Mode::Fixed(_) => "fixed",
            Mode::Full => "full",
        };
        Record { mode: mode.to_string(), root, own: st.own.clone() }
    }

    /// 从对话存档里还原工作区（打开旧对话时用）。档位名认不出就退回「默认」，不猜。
    ///
    /// 两条都关键：
    /// - **`default` 档的根若已存档就沿用**（`At` + `picked = false`），不要等首条消息
    ///   现建一块 —— 那是「每次进入同一个对话都是不同的工作区」的原因；
    /// - **`own` 也要一起还原** —— 少了它，「重开这段对话 → 切完全访问 → 切回默认」
    ///   又会新建一块，也就是工作区累加的另一半来源。
    pub fn restore(&self, rec: &Record) {
        let mut st = self.lock();
        st.own = rec.own.clone();
        match (rec.mode.as_str(), rec.root.clone()) {
            ("full", _) => {
                st.active = Active::Full;
                st.started = true;
                st.picked = false; // 没有根，「点名挑的」无从谈起
            }
            // 用户当年点名的地 ⇒ 界面报「选择工作区」。
            ("fixed", Some(p)) => {
                st.active = Active::At(normalize(&p));
                st.started = true;
                st.picked = true;
            }
            // `default` 档的地：沿用存档的根，但档位仍是「默认」。
            ("default", Some(p)) => {
                let p = normalize(&p);
                st.active = Active::At(p.clone());
                st.started = true;
                st.picked = false;
                // 老格式的记录没有 `own` 字段：`root` 就是那块自建地，拿它兜底。
                if st.own.is_none() {
                    st.own = Some(p);
                }
            }
            // `default` 没给根、或档位名认不出：回「还没定」，首条消息现建一块。
            _ => {
                st.active = Active::Pending;
                st.started = false;
                st.picked = false;
            }
        }
    }

    /// 开始一段新对话：**一律回到「默认」档**，并把「已开始」清掉。
    ///
    /// 用户 2026-10-04 定的语义：「新建对话，默认工作区应该是默认才对」——
    /// 上一段对话里临时切过的档（`fixed` / `full`）**不再继承**。原先只在
    /// `cfg.mode == "default"` 时才重置，于是「上一段对话挑了某个目录、点 + 之后新对话
    /// 还在那个目录里」，看着就像新对话根本没换地。
    ///
    /// ⚠️ **代价**（用户已确认接受）：配置里写 `mode = "fixed"` 的用户，只有
    /// **本次启动的第一段对话**落在配置点名的那块地；之后每段新对话都是「默认」，
    /// 会在 `workplace/` 下另开一块新地。想让固定地一直生效，得每段对话切一次。
    ///
    /// **不在这里建目录** —— `default` 档位的目录要等**首条消息**才建
    /// （用户点了「+」但没说话时，不该在磁盘上留下一个空目录）。
    pub fn reset_for_new_conversation(&self) {
        let mut st = self.lock();
        st.started = false;
        st.active = Active::Pending;
        st.picked = false;
        // 新对话要另开一块地：把「本对话自建的地」也清掉，否则它会去复用**上一段对话**
        // 那块（那正是「新对话还带着上一个工作区」的另一半来源）。
        st.own = None;
    }

    /// 本对话开始前确保根已定。返回 `Some(目录)` 表示**这正是本次调用建的**
    /// （或本对话第一次确定），调用方应当用一条 `Info` 告知用户。
    ///
    /// 「每段对话一块新地」的语义就落在这里：`started` 保证一段对话只建一次。
    pub fn ensure_started(&self, app_root: &Path) -> anyhow::Result<Option<PathBuf>> {
        let mut st = self.lock();
        if st.started {
            return Ok(None);
        }
        // `Pending`：**唯一**需要真正动手建目录的分支，单独拎出来处理 ——
        // 免得在借用 `st.active` 的期间又去写 `st.active`（借用检查器不让过，
        // 而且把「读状态」与「写状态」分开读起来也更清楚）。
        if matches!(st.active, Active::Pending) {
            let dir = normalize(&unique_dir(&Self::workdir_of(app_root), &stamp()));
            std::fs::create_dir_all(&dir)
                .map_err(|e| anyhow::anyhow!("创建工作区失败（{}）: {e}", dir.display()))?;
            st.active = Active::At(dir.clone());
            // 现建的地 ⇒ 报「默认」而不是「选择工作区」（判据是 `picked`，见 `State`）。
            st.picked = false;
            // 记成「本对话自建的地」：此后无论切到哪一档、再切回默认，都是这一块
            // （切一次就新建一个目录的话，`workplace/` 会被空目录堆满）。
            st.own = Some(dir.clone());
            st.started = true;
            return Ok(Some(dir));
        }
        // `Full` / `At`：本对话第一次确定根，只记一下。`At` 的根要报回调用方，
        // 好让它用一条 `Info` 告诉用户「这块地就是本次的工作区」。
        let root = match &st.active {
            Active::At(p) => Some(p.clone()),
            _ => None,
        };
        st.started = true;
        Ok(root)
    }

    /// 运行期切档位（CLI 的 `/workplace`、GUI 的按钮）。返回一句给用户看的结果。
    pub fn set_mode(&self, mode: Mode, app_root: &Path) -> anyhow::Result<String> {
        // 切到「选择工作区」时**立刻建**：用户明确挑了一块地，就该有那块地
        // （与配置里 fixed 档同一套语义）。相对路径相对**应用根**解释 ——
        // 与 `new()` 里 `fixed` 档的处理保持一致，免得同一个相对路径在
        // 配置里与 CLI 上落到两个地方。
        let dir = match mode {
            Mode::Fixed(ref p) => {
                // 相对路径按**应用根**解（与 `new()` 里 `fixed` 档同一套）。
                let p = if p.is_absolute() { p.clone() } else { app_root.join(p) };
                Some(normalize(&p))
            }
            _ => None,
        };
        if let Some(d) = dir.as_ref() {
            std::fs::create_dir_all(d)
                .map_err(|e| anyhow::anyhow!("目录 {} 无法使用: {e}", d.display()))?;
            if !d.is_dir() {
                anyhow::bail!("{} 不是一个目录", d.display());
            }
        }
        let mut st = self.lock();
        match mode {
            Mode::Full => {
                st.active = Active::Full;
                st.started = true;
                st.picked = false; // 没有根，「点名挑的」无从谈起
                Ok("已切到「完全访问」：AOX 可以读写设备上的任何文件（含工作区之外）。".into())
            }
            Mode::Fixed(_) => {
                let d = dir.expect("上面已经算过");
                st.active = Active::At(d.clone());
                st.started = true;
                st.picked = true; // 用户点名的地 ⇒ 界面报「选择工作区」
                Ok(format!("已切到「选择工作区」：{}", d.display()))
            }
            Mode::Default => {
                st.picked = false;
                let _ = app_root;
                // **本对话已经为「默认」建过一块地 ⇒ 回到那一块，不再另建。**
                // 见 `State::own`：切到「完全访问」再切回来就新建一个目录的话，
                // `workplace/` 会被空目录堆满（用户 2026-10-05 报的）。
                // 只有本对话还没建过，才回到「还没定」，等首条消息现建。
                match st.own.clone() {
                    Some(p) => {
                        st.active = Active::At(p.clone());
                        st.started = true;
                        Ok(format!("已切到「默认」：本对话仍在 {} 里。", p.display()))
                    }
                    None => {
                        st.active = Active::Pending;
                        st.started = false;
                        Ok("已切到「默认」：下一条消息发出时会在 workplace/ 下开一块新工作区。"
                            .into())
                    }
                }
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // 判定发生在工具调用路径上，持锁时间是一次路径比较 —— 不会中毒。
        // 真中毒了也不该 panic 打死整个 agent：退回「不受限」比让程序挂掉好，但要出声。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前是否**不受限**（`full` 档 = 一切放行）。
    ///
    /// 给判定点做短路用。这一档下 `check` 算出来的东西**整个会被丢掉**（调用方只要
    /// `.err()`），却要实打实付一次到几次 `canonicalize` —— 实测 65~224 µs，走的是文件系统。
    /// 判定在**每一次工具调用**上都跑，而 `full` 档（`mode = "full"`，或界面上切了
    /// 「完全访问」）下这份开销纯属白花；调用方先问一句这个，整条路径（含 `scan_paths`
    /// 那张粗网）就跳过了。
    ///
    /// ⚠️ 2026-10-05 起**配置默认**是 `default`（受限），不再是 `full` —— 所以这条短路
    /// 现在只对显式选了完全访问的用户生效，别再把「大多数用户走这条」当理由。
    ///
    /// ⚠️ 与 [`Workplace::mode`] 同一个规矩：**不要在持锁的上下文里调它**。
    pub fn is_unrestricted(&self) -> bool {
        matches!(self.lock().active, Active::Full)
    }

    /// 判定一个「**落盘**」路径（写 / 删）。`Ok` = 规格化后的绝对路径，可直接交给文件 API。
    ///
    /// 等价于 [`Workplace::check_access`]`(raw, Access::Write)`。保留这个名字是因为
    /// 它是「受限」的默认含义 —— 调用方不特别说明时，要按最严的那档判。
    pub fn check(&self, raw: &str) -> Result<PathBuf, Reject> {
        self.check_access(raw, Access::Write)
    }

    /// 判定一个「**只读**」路径（读文件 / 列目录）。
    ///
    /// 按 [`Access::Read`] 的语义，它**不受工作区限制**，所以**从不返回 `Err`** ——
    /// 仍然返回 `Result` 只是为了与 [`Workplace::check`] 同形：两层判定点（agent 与工具）
    /// 用的是同一段调用代码，读/写只差一个 `Access`；将来若要收紧读，改动也只落在这里。
    pub fn check_read(&self, raw: &str) -> Result<PathBuf, Reject> {
        self.check_access(raw, Access::Read)
    }

    /// 两种意图的**唯一**实现。
    ///
    /// - `Access::Read`：三种状态（`Full` / `Pending` / `At`）**一律放行**，只做规格化。
    ///   刻意连 `Pending` 都不拒 —— 受限档下「地还没开」挡的是落盘，不该顺手把
    ///   「读设备上任何一份文件」也一起禁掉（那正是新语义要开的那条路）。
    /// - `Access::Write`：`Full` 放行；`Pending` 全拒（见 [`Active::Pending`]）；
    ///   `At` 只认**区内绝对路径**（相对路径一律拒，见模块文档「① 只看绝对路径」）。
    pub fn check_access(&self, raw: &str, access: Access) -> Result<PathBuf, Reject> {
        if access == Access::Read {
            return Ok(normalize(Path::new(raw)));
        }
        let root = match &self.lock().active {
            Active::Full => return Ok(normalize(Path::new(raw))),
            Active::Pending => return Err(Reject::NotStarted),
            Active::At(r) => r.clone(),
        };
        let p = Path::new(raw);
        if !p.is_absolute() {
            // 见模块文档「① 只看绝对路径」。
            return Err(Reject::Relative { root, raw: raw.to_string() });
        }
        let resolved = normalize(p);
        if !inside(&root, &resolved) {
            return Err(Reject::Outside { root, raw: raw.to_string(), resolved });
        }
        Ok(resolved)
    }

    /// `/workplace` 的正文。
    pub fn report(&self, app_root: &Path) -> String {
        // ⚠️ **顺序是刻意反的，别顺手「优化」成「先取锁再算档位名」。**
        // `std::sync::Mutex` 不可重入：先 `self.mode()`（它自己锁一次 state）拿档位名、
        // **再**取状态快照，是对的；反过来在持锁期间调 `self.mode()` 就会**整进程死锁** ——
        // 现象是「切到固定工作区之后再看一次 /workplace 就没反应了」。上一版正是这样。
        let label = self.mode().label();
        let active = self.lock().active.clone();
        let mut out = String::from("工作区\n\n");
        match &active {
            Active::Full => {
                out.push_str(
                    "  档位：完全访问\n  \
                     AOX 可以读写设备上的任何文件，不设边界。\n\n  \
                     这是**显式**选的（config.toml 里写了 mode = \"full\"，或界面上切过）—— \
                     配置默认是「默认」档（受限）。\n  想收紧：/workplace default 或 /workplace <目录>\n",
                );
            }
            Active::Pending => {
                out.push_str(&format!(
                    "  档位：默认（受限）\n  \
                     根：还没定 —— 下一条消息发出时会在 {} 下开一块新工作区。\n",
                    Self::workdir_of(app_root).display()
                ));
            }
            Active::At(p) => {
                out.push_str(&format!(
                    "  档位：{label}（受限）\n  根：{}\n  \
                     写入与删除都被限制在这个目录里；**读取不受限**（整个设备都能读）。\n",
                    p.display()
                ));
            }
        }
        out.push_str(
            "\n  ⚠️ 边界只管「宿主提供的能力」：宿主内置的文件工具，以及插件工具的路径参数。\n     \
             它管不了一个不守规矩的插件自己 open() 文件 —— 那要 OS 级隔离（AppContainer）。\n",
        );
        out
    }
}

// ─────────────────────────── /workplace 命令 ───────────────────────────

/// 本命令的两个名字：**第一个是正式名，后面的都是兼容旧名。**
///
/// 2026-10-04 统一过一次命名 —— 这个词一律是 `workplace`（目录名 `<应用根>/workplace/`、
/// 配置段 `[workplace]`、CLI 命令 `/workplace`，连 Rust 的模块与类型也在第二轮跟着改了）。
/// 旧名「/workspace」是统一之前的那个，**留着**：用户有肌肉记忆，而这条命令一旦失效，
/// 他看到的是「档位切不动」而不是「命令改名了」，几乎不可能自己查出来。
/// 不写进 `/help`，当隐式别名。
const CMD_NAMES: [&str; 2] = ["/workplace", "/workspace"];

/// `/workplace` 命令族的判定（CLI 的路由用，见 `main.rs` 里那串 `_ if ...::is_cmd(line)`）。
///
/// **判定与执行都放在这里**而不是 CLI 里，是为了让「切档位」只有一份实现、一份文案 ——
/// 将来 GUI 若也要一条 `/workplace` 输入路径，直接复用即可，不会出现
/// 「CLI 说切成功了、GUI 那边没切」这类两套逻辑的错位。
pub fn is_cmd(line: &str) -> bool {
    cmd_arg(line).is_some()
}

/// 剥出命令后面的参数；`None` = 这一行根本不是本命令。
///
/// 用「按名匹配 + 剥一次前缀」而不是 `trim_start_matches`：后者会把前缀**反复**剥掉，
/// 于是目录名叫 `/workplace`（或旧名「/workspace」）时，`/workplace /workplace` 这种完全合法的
/// 写法会被剥成空串 —— 那正好等于「不带参数」，用户拿到的是现状而不是切换结果。
fn cmd_arg(line: &str) -> Option<&str> {
    for name in CMD_NAMES {
        if line == name {
            return Some("");
        }
        if let Some(rest) = line.strip_prefix(name) {
            // 只认「命令名 + 空格」这一种，避免 `/workplaceX` 也被当成 `/workplace X`。
            if let Some(arg) = rest.strip_prefix(' ') {
                return Some(arg.trim());
            }
        }
    }
    None
}

/// 执行 `/workplace` 命令。
///
/// - `/workplace`（不带参数）→ 打印当前档位与根
/// - `/workplace full` → 完全访问（不受限）
/// - `/workplace default` → 每段对话一块新地（**下一条消息**生效）
/// - `/workplace <目录>` → 固定到该目录（相对路径相对**应用根**解释）
pub fn command(rt: &std::sync::Arc<crate::runtime::Runtime>, line: &str) -> anyhow::Result<String> {
    // `is_cmd` 已经保证这行是本命令，所以取不到 `None`；真取不到就按「不带参数」办。
    let arg = cmd_arg(line).unwrap_or("");
    if arg.is_empty() {
        return Ok(rt.workplace.report(&rt.root));
    }
    let mode = if arg.eq_ignore_ascii_case("full") {
        Mode::Full
    } else if arg.eq_ignore_ascii_case("default") {
        Mode::Default
    } else {
        Mode::Fixed(PathBuf::from(arg))
    };
    rt.workplace.set_mode(mode, &rt.root)
}

// ─────────────────────────── 越界契约 ───────────────────────────

/// 一次越界。文案是**契约**，不是日志 —— 分 [`model_text`](Reject::model_text)
/// 与 [`user_text`](Reject::user_text) 两份，因为读者不同、要的东西也不同。
#[derive(Debug, Clone)]
pub enum Reject {
    /// 受限档位下给了相对路径。
    Relative { root: PathBuf, raw: String },
    /// 路径落在工作区之外。
    Outside { root: PathBuf, raw: String, resolved: PathBuf },
    /// 受限档位，但本对话还没开始（根还没定）。
    NotStarted,
}

impl Reject {
    /// 给**模型**看的文本。三段是硬要求：
    ///
    /// ① 发生了什么（哪个路径、根在哪）② 这是权限边界不是工具坏了、**别重试**
    /// ③ 请转述给用户并告诉他怎么提权。
    ///
    /// 为什么必须写「别重试」：模型面对一次工具失败的本能是换个写法再试一次
    /// （`./x` → `x` → `../x`），而越界是**换不掉**的 —— 不写明就会一路烧轮次，
    /// 而且它还会以为是自己参数写错了。
    pub fn model_text(&self) -> String {
        // 每一支都要**自带根**：模型拿到的就这一段文本，它没法再去别处查。
        let what = match self {
            Reject::Relative { root, raw } => format!(
                "你给的是相对路径 {raw:?}，本档位不接受相对路径。\
                 要改写成工作区内的**绝对路径**，工作区根是：{}。\
                 原因：宿主与插件子进程的工作目录不是同一个，同一串相对路径在两边会指到\
                 两个不同文件 —— 那种不一致比多写几个字符危险得多。",
                root.display()
            ),
            Reject::Outside { root, raw, resolved } => format!(
                "{raw:?} 解析为 {}，它落在工作区之外（工作区根：{}）。",
                resolved.display(),
                root.display()
            ),
            Reject::NotStarted => {
                "本档位下这段对话的工作区还没确定（它在用户发出第一条消息时创建），\
                 所以还不能往工作区里写东西。"
                    .to_string()
            }
        };
        // ⚠️ 下面这段是**给模型的操作说明**，它会被原样转述给用户 ——
        // 所以里面只能写**真的存在**的入口。两条前端入口都要写全：
        // CLI 的 `/workplace`，以及 GUI 输入框左下角那枚「工作区」按钮（三档 + 目录选择器，
        // 2026-10-04 落地）。漏掉哪一条，走那条前端的用户就会收到一份「按不了的建议」。
        //
        // 还有一条容易漏的（新语义）：**读不受工作区限制**。不说清的话，模型读到
        // 「被权限边界拦下」会连「读边界外的东西」也一起放弃、甚至去请求提权 ——
        // 而那条路根本不需要提权。
        format!(
            "【权限边界】{what}\n\n\
             这不是工具坏了，也不是参数写错了 —— 是当前工作区的权限边界。\
             **不要重试这次调用，也不要换一种写法绕过它**（同一个越界目标怎么试都会被拒）。\n\n\
             请把这个情况**转述给用户**：当前工作区只允许在边界之内**写入和删除**，\
             所以你无法改动那个位置。注意：**读文件不受工作区限制** —— \
             如果只是要读取边界之外的内容，直接读就可以了，不必提权。\
             如果用户确实需要写或删那个位置，请告诉他可以这样提升权限：\n  \
             - 界面：点输入框左下角的「工作区」，选「默认 / 选择工作区 / 完全访问」\n  \
             - 命令行：`/workplace default`（每段对话一块新地）、`/workplace <目录>`（指定目录）\
             或 `/workplace full`（完全访问，AOX 可读写设备上的任何文件）\n  \
             - 或改 config.toml 的 `[workplace]`（mode = \"full\" / \"default\" / \"fixed\"）后重启\n\n\
             在用户改变权限之前，请改用工作区内可行的方案完成剩下的部分，\
             或者明确告诉用户这一步做不了、还需要什么。"
        )
    }

    /// 给**用户**看的一行（调用方用 `Warn` 事件发出去）。
    ///
    /// 与 `model_text` 分开写：模型要的是「别重试 + 怎么提权」，用户要的是
    /// 「AOX 刚才想干什么、被什么挡住了」。同一段文字硬塞给两边，两边都不好用。
    pub fn user_text(&self) -> String {
        match self {
            Reject::Relative { root, raw } => format!(
                "已拦下一次越界访问：相对路径 {raw:?}（工作区根 {}）。\
                 本档位只接受工作区内的绝对路径。",
                root.display()
            ),
            Reject::Outside { root, raw, resolved } => format!(
                "已拦下一次越界访问：{raw:?} → {}，工作区是 {}。\
                 本次调用没有执行。（要放宽用 `/workplace default|<目录>|full`，\
                 或改 config.toml 的 [workplace]）",
                resolved.display(),
                root.display()
            ),
            Reject::NotStarted => {
                "工作区还没建立，这次写入/删除被拦下 —— 发一条消息后即可使用。".to_string()
            }
        }
    }
}

// ─────────────────────────── 参数扫描（粗网）───────────────────────────

/// 从一次调用的参数里挑出「像路径」的字符串，返回 `(参数名, 值)`。
///
/// **这是张粗网，只用于插件工具**：插件是黑盒，宿主只能按参数名猜。
/// 宿主自己的文件工具不走这里 —— 它们的 schema 是我们定义的，直接读 `path`（见
/// [`PATH_ARG`]）。误判会拒掉一次合法调用，所以名单刻意保守。
pub fn scan_paths(args: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(args, "", &mut out);
    out
}

fn walk(v: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match val {
                    Value::String(s) => {
                        if looks_like_path(k, s) {
                            out.push((key, s.clone()));
                        }
                    }
                    // 字符串数组：`paths = ["a", "b"]` 这种写法很常见，逐项判。
                    // 元素名沿用数组名（`paths[0]` 之类的位置信息对模型没用）。
                    Value::Array(items) => {
                        for item in items {
                            if let Value::String(s) = item {
                                if looks_like_path(k, s) {
                                    out.push((key.clone(), s.clone()));
                                }
                            } else {
                                walk(item, &key, out);
                            }
                        }
                    }
                    _ => walk(val, &key, out),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, prefix, out);
            }
        }
        _ => {}
    }
}

/// 值超过这个长度、或含换行，就**不再当成路径**（见 [`looks_like_path`]）。
///
/// 260 = Windows 传统的路径上限（`MAX_PATH`）。取它而不是别的数，是因为超过这个
/// 长度的**参数值**几乎不可能是路径，只可能是源码 / 正文 / 日志。
const MAX_PATH_CHARS: usize = 260;

fn looks_like_path(key: &str, val: &str) -> bool {
    if val.trim().is_empty() {
        return false;
    }
    // ① 参数名像路径 —— 最可靠的一条。键名是插件**自己声明**的，它说这是路径就是路径，
    //    值再长也照判（`path` 里真可能是条 300 字符的长路径）。
    if PATH_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
        return true;
    }
    // ② 键名没说话，就只能靠值的形态猜 —— 那先排除「一眼不是路径」的值：
    //    含换行、或长过一个路径上限。
    //    **这一条是必需的，不是洁癖**：插件工具的参数里常带整段源码 / 正文
    //    （`create_plugin` 的 `source` 就是），那种字符串里几乎必然出现 `/`、`\` 或
    //    `..` —— 光看值形态会把 2 KB 的源码整段当成一个路径，后果是受限档下
    //    `create_plugin` 直接被拒（**功能坏了，不是慢**）、full 档每次白扫一遍。
    //    用 `chars().count()` 而不是 `len()`：源码里的中文按字节算会提前触发闸门，
    //    而路径长度是按字符数的。
    if val.contains('\n') || val.contains('\r') || val.chars().count() > MAX_PATH_CHARS {
        return false;
    }
    // ③ 值本身明显是绝对路径（`/x`、`\x`、`X:\x`、`\\server\share`、`~/x`）
    let b = val.as_bytes();
    if b[0] == b'/' || b[0] == b'\\' || val.starts_with("~/") {
        return true;
    }
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return true;
    }
    // ③ 含 `..` 分量 —— 这个词几乎只出现在路径里，而且它正是越界的常见写法
    val.split(['/', '\\']).any(|seg| seg == "..")
}

// ─────────────────────────── 路径规格化与包含判定 ───────────────────────────

/// 规格化：解开 symlink / 8.3 短名 / 大小写，并消掉 `.` 与可消的 `..`。
///
/// 对**还不存在**的路径（写文件时最常见）不能直接 `canonicalize`，所以退化成
/// 「把最深的已存在祖先 canonicalize，再把剩下的尾巴拼回去」。
/// 这一步是必须的：不这么做，`<工作区>/../..` 或一个指向别处的 symlink
/// 就能让前缀比较得出错误结论。
fn normalize(p: &Path) -> PathBuf {
    let lex = lexical(p);
    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = lex.clone();
    loop {
        if let Ok(c) = cur.canonicalize() {
            // `canonicalize` 在 Windows 上返回 **verbatim 路径**（`\\?\C:\…`）—— 它才是
            // 「解开 symlink / 8.3 短名」之后的那一份，判定必须用它；但把它**展示**出去
            // （`/workplace`、越界文案、给模型的契约）会让用户以为出了错，模型照抄进工具
            // 调用里也很难受。所以在**规格化的唯一出口**这里摘掉前缀：全模块的路径都从
            // `normalize` 出来，于是判定与展示看到的是同一套形状，不会出现
            // 「比较用它、显示用另一个」这种错位（`inside` 两侧也都是这个形状）。
            let mut out = strip_verbatim(c);
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return lexical(&out);
        }
        let Some(name) = cur.file_name().map(|s| s.to_os_string()) else { break };
        tail.push(name);
        match cur.parent() {
            // `parent()` 返回空 / 等于自己 ⇒ 到顶了（驱动器的根、或相对路径的起点）
            Some(parent) if !parent.as_os_str().is_empty() && parent != cur => {
                cur = parent.to_path_buf();
            }
            _ => break,
        }
    }
    lex
}

/// 摘掉 Windows 的 verbatim 前缀：`\\?\C:\x` → `C:\x`、`\\?\UNC\s\share` → `\\s\share`。
///
/// 只在 [`normalize`] 的出口调一次。代价是超过 `MAX_PATH`（260 字符）的路径会失去
/// verbatim 那层豁免 —— 本项目的场景（工作区、history、plugins 全在应用根附近）
/// 根本到不了那个长度，而「每条边界文案里带着一串 `\\?\`」是天天能看见的。
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

/// 纯词法规格化：只消 `.` 与可消的 `..`，**不碰文件系统**（也不解开 symlink）。
fn lexical(p: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut stack: Vec<OsString> = Vec::new();
    for c in p.components() {
        match c {
            // 盘符与根直接落在结果头部，且**不可被 `..` 弹掉** —— 否则
            // `C:\..\..\x` 会变成 `x`（相对路径），判定直接失效。
            Component::Prefix(_) | Component::RootDir => prefix.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    stack.push("..".into());
                }
            }
            Component::Normal(s) => stack.push(s.to_os_string()),
        }
    }
    let mut out = prefix;
    for s in stack {
        out.push(s);
    }
    out
}

/// `p` 是否落在 `root` 之内（含等于 root）。
///
/// Windows 上逐段**大小写不敏感**比较（`C:` 与 `c:`、`Users` 与 `users` 是同一处）。
/// 非 ASCII 的大小写折叠没做 —— 那种目录名极少出现在路径前缀里，而真要做对得上 Unicode 表。
fn inside(root: &Path, p: &Path) -> bool {
    let mut r = root.components();
    let mut q = p.components();
    loop {
        match (r.next(), q.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(a), Some(b)) => {
                let (a, b) = (a.as_os_str().to_string_lossy(), b.as_os_str().to_string_lossy());
                let same = if cfg!(windows) { a.eq_ignore_ascii_case(&b) } else { a == b };
                if !same {
                    return false;
                }
            }
        }
    }
}

// ─────────────────────────── 时间戳 ───────────────────────────

/// `<YYYYMMDDHHMMSS>` 本地时间戳（工作区目录名）。
///
/// 与 `store::jsonl` 的文件名走同一套口径（本地时间、零新增 crate）。这里只有
/// 一次 `GetLocalTime` 调用，没有共享的逻辑 —— 为了十行 FFI 去把存储后端的时间
/// 辅助函数抽公共层，改动面比收益大。
fn stamp() -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::SYSTEMTIME;
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut st) };
        format!(
            "{:04}{:02}{:02}{:02}{:02}{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
    #[cfg(not(windows))]
    {
        // 拿不到本地时区就如实按 UTC 记，不拿「东八区近似」骗人（与 store 同一口径）。
        let secs = crate::history::unix_now();
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "{y:04}{m:02}{d:02}{:02}{:02}{:02}",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    }
}

/// 在 `base` 下挑一个还没被占用的目录名：`<stamp>`，撞了就 `<stamp>-2`、`<stamp>-3`…
///
/// 为什么需要它：时间戳只精确到秒，而「同一秒里连开两段新对话」在快速点击或脚本化
/// 驱动时真会发生。撞上时 `create_dir_all` 不会报错 —— 它会把第二段对话**悄悄塞进**
/// 第一段的地里，两块地变成一块，而用户以为它们是分开的。`-N` 后缀是最便宜的去重：
/// 正常情况（不同秒）永远走不到这里，名字里也不会多出无意义的随机数。
fn unique_dir(base: &Path, stamp: &str) -> PathBuf {
    let first = base.join(stamp);
    if !first.exists() {
        return first;
    }
    for n in 2..1000u32 {
        let cand = base.join(format!("{stamp}-{n}"));
        if !cand.exists() {
            return cand;
        }
    }
    // 一千个都没空位：基本不可能（除非有人手工塞满）；照旧返回第一个，
    // 让它去 `create_dir_all` 里正常成功 —— 静默复用比在这里 panic 好。
    first
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkplaceConfig;

    fn ws(mode: &str, root: Option<&str>) -> Workplace {
        let app = std::env::temp_dir().join("aox-ws-test");
        Workplace::new(WorkplaceConfig { mode: mode.into(), root: root.map(str::to_string) }, &app)
            .expect("测试用配置应当合法")
    }

    fn fixed(dir: &str) -> Workplace {
        ws("fixed", Some(dir))
    }

    /// 命令的两个名字：`/workplace` 是正式名，`/workspace` 是**兼容旧名**（2026-10-04 统一命名）。
    ///
    /// 钉住三件事：① 两个名字都认；② 参数照剥（`/workplace default` → `default`）；
    /// ③ 目录名恰好叫 `/workplace` 时，`/workplace /workplace` **不能**被剥成空串 ——
    /// 那是老写法 `trim_start_matches("/workplace")` 会犯的错（它会反复剥前缀），
    /// 用户拿到的是「现状」而不是「切换结果」，而且毫无提示。
    #[test]
    fn command_answers_to_both_names_and_parses_args() {
        for name in ["/workplace", "/workspace"] {
            assert!(is_cmd(name), "{name} 应被认成命令");
            assert_eq!(cmd_arg(name), Some(""), "{name} 不带参数时参数应为空");
            assert_eq!(cmd_arg(&format!("{name} default")), Some("default"));
            assert_eq!(cmd_arg(&format!("{name}   C:\\x ")), Some("C:\\x"), "多余空格要吃掉");
        }
        assert_eq!(cmd_arg("/workplace /workplace"), Some("/workplace"), "目录名与命令名同名也要剥对");
        assert_eq!(cmd_arg("/workspace /workspace"), Some("/workspace"));
        for other in ["/workplaceX", "/workplaces", "/work", "workplace", ""] {
            assert!(!is_cmd(other), "{other:?} 不该被认成这条命令");
        }
    }

    #[test]
    fn unknown_mode_is_an_error() {
        let app = std::env::temp_dir();
        let r = Workplace::new(WorkplaceConfig { mode: "sandbox".into(), root: None }, &app);
        assert!(r.is_err(), "认不出的 mode 必须启动即报错，不静默退回 full");
    }

    #[test]
    fn fixed_without_root_is_an_error() {
        let app = std::env::temp_dir();
        let r = Workplace::new(WorkplaceConfig { mode: "fixed".into(), root: None }, &app);
        assert!(r.is_err(), "fixed 档没给 root 必须报错");
    }

    #[test]
    fn full_lets_everything_through() {
        let w = ws("full", None);
        assert!(w.check("C:\\Windows\\System32").is_ok());
        assert!(w.check("relative.txt").is_ok(), "不受限档位不该对相对路径挑刺");
        assert_eq!(w.active_root(), None);
    }

    /// 受限档位：**相对路径一律拒绝**，且文案要给出根（模型靠它改写）。
    #[test]
    fn confined_rejects_relative_and_tells_the_root() {
        let base = std::env::temp_dir().join("aox-ws-rel");
        std::fs::create_dir_all(&base).unwrap();
        let w = fixed(&base.to_string_lossy());
        let e = w.check("config.toml").expect_err("相对路径必须被拒");
        let t = e.model_text();
        assert!(t.contains("相对路径"), "{t}");
        assert!(t.contains(&normalize(&base).display().to_string()), "{t}");
        assert!(t.contains("转述给用户"), "契约必须要求模型转述：{t}");
        assert!(t.contains("不要重试"), "契约必须写明别重试：{t}");
    }

    #[test]
    fn confined_allows_inside_and_rejects_outside() {
        let base = std::env::temp_dir().join("aox-ws-in");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let w = fixed(&base.to_string_lossy());

        let inside = base.join("sub").join("a.txt");
        assert!(w.check(&inside.to_string_lossy()).is_ok(), "工作区内的绝对路径应放行");

        let outside = base.parent().unwrap().join("aox-ws-outside.txt");
        let e = w.check(&outside.to_string_lossy()).expect_err("工作区外必须被拒");
        match e {
            Reject::Outside { .. } => {}
            other => panic!("应当是 Outside，实际 {other:?}"),
        }
    }

    /// `..` 逃逸：词法上看着像在里面，规格化后必须落在外面。
    #[test]
    fn dotdot_escape_is_caught() {
        let base = std::env::temp_dir().join("aox-ws-dotdot");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let w = fixed(&base.to_string_lossy());

        let escape = base.join("sub").join("..").join("..").join("evil.txt");
        let e = w.check(&escape.to_string_lossy()).expect_err("`..` 逃逸必须被拒");
        assert!(matches!(e, Reject::Outside { .. }), "{e:?}");

        // 没逃出去的那种 `..` 要放行（不能把 `sub/../ok.txt` 也一起拒了）
        let ok = base.join("sub").join("..").join("ok.txt");
        assert!(w.check(&ok.to_string_lossy()).is_ok(), "留在工作区内的 `..` 应当放行");
    }

    /// 大小写不敏感（Windows 语义）：路径前缀大小写不同不算越界。
    #[test]
    fn prefix_compare_ignores_ascii_case() {
        if !cfg!(windows) {
            return;
        }
        let base = std::env::temp_dir().join("AOX-WS-CASE");
        std::fs::create_dir_all(&base).unwrap();
        let w = fixed(&base.to_string_lossy());
        let shouty = base.join("A.TXT");
        let lower = PathBuf::from(shouty.to_string_lossy().to_ascii_lowercase());
        assert!(w.check(&lower.to_string_lossy()).is_ok(), "大小写不同不该判越界");
    }

    /// `default` 档位：**根没定之前一律拒绝**，不能因为「还没建」而静默全放行。
    #[test]
    fn pending_is_rejected_not_silently_allowed() {
        let app = std::env::temp_dir().join("aox-ws-pending");
        let w = ws("default", None);
        let e = w.check("C:\\Windows").expect_err("根没定就该拒，而不是放行");
        assert!(matches!(e, Reject::NotStarted), "{e:?}");

        let dir = w.ensure_started(&app).unwrap().expect("应当建出一块新地");
        assert!(dir.is_dir(), "目录应当真的建出来了");
        // 用 `normalize` 比前缀：`ensure_started` 交回的是规格化过的路径，
        // 而 `temp_dir()` 给的那份可能还带着 8.3 短名 / 大小写差异（`canonicalize` 之后
        // 就成了另一串字符），直接 `starts_with` 会假红。
        assert!(dir.starts_with(normalize(&app.join(WORKPLACE_DIR))));
        assert!(w.check(&dir.join("x.txt").to_string_lossy()).is_ok());
        // 一段对话只建一次
        assert!(w.ensure_started(&app).unwrap().is_none(), "同一段对话不该重复建目录");
        // 新对话才换一块地
        w.reset_for_new_conversation();
        let dir2 = w.ensure_started(&app).unwrap().unwrap();
        assert_ne!(dir, dir2, "新对话应当换一块新地");
        let _ = std::fs::remove_dir_all(app.join(WORKPLACE_DIR));
    }

    #[test]
    fn scan_paths_picks_path_like_values_only() {
        let args = serde_json::json!({
            "path": "config.toml",
            "text": "这里有个 / 斜杠但不是路径",
            "pattern": "*.rs",
            "abs": "/etc/passwd",
            "win": "D:\\data\\x.csv",
            "escape": "sub/../../etc/passwd",
            "nested": {"dir": "plugins"},
            "paths": ["a.txt", "b/c.txt"],
            "url": "https://example.com/x",
            "n": 3
        });
        let hits: Vec<String> = scan_paths(&args).into_iter().map(|(_, v)| v).collect();
        assert!(hits.contains(&"config.toml".to_string()), "path 参数要被抓到");
        assert!(hits.contains(&"/etc/passwd".to_string()), "绝对路径要被抓到");
        assert!(hits.contains(&"D:\\data\\x.csv".to_string()), "Windows 绝对路径要被抓到");
        assert!(hits.contains(&"plugins".to_string()), "嵌套的 dir 要被抓到");
        // 数组里的相对名靠**键名**（`paths`）抓到 —— 键名不在名单里的话，光看值
        // 是认不出 `b/c.txt` 的，而认不出来正是这张粗网「宁漏勿误伤」的代价。
        assert!(hits.contains(&"a.txt".to_string()), "paths 数组里的项要被抓到");
        assert!(hits.contains(&"b/c.txt".to_string()), "同上");
        assert!(!hits.contains(&"*.rs".to_string()), "通配符不是路径");
        assert!(!hits.contains(&"3".to_string()));
        // 含 `/` 但不是路径：URL 绝不能被抓 —— 一旦误判，受限档位下插件连取数据都做不了
        assert!(!hits.contains(&"https://example.com/x".to_string()), "URL 不是文件路径");
        // 键名不像路径、值又带着空格与正文 ⇒ 不判（粗网宁可漏，不可误伤）
        assert!(!hits.iter().any(|h| h.contains("斜杠")), "正文不该被当路径");
    }

    /// 用户那一侧的文案必须说清「想干什么、被什么挡住」，而不是照抄给模型的那份。
    #[test]
    fn user_text_differs_from_model_text() {
        let w = fixed(&std::env::temp_dir().to_string_lossy());
        let e = w.check("relative.txt").unwrap_err();
        let (m, u) = (e.model_text(), e.user_text());
        assert!(u.contains("已拦下一次越界访问"), "{u}");
        assert!(!u.contains("不要重试"), "给用户的那份不该带「别重试」这种对模型说的话");
        assert!(m.len() > u.len(), "给模型的契约要比给用户的一行更长更完整");
    }

    /// 回归：`report()` 曾经在**持有状态锁**的期间又调 `self.mode()`（它也要锁同一把
    /// `std::sync::Mutex`）—— `Mutex` 不可重入，于是「切到固定工作区之后再执行一次
    /// `/workplace`」把整个进程**钉死**（实机上是 exit=124，超时被杀）。
    ///
    /// 用「另起线程 + 带超时的 recv」来钉它：真死锁时这里是**测试失败**，
    /// 而不是让 `cargo test` 一起挂住 —— 挂住的测试比失败的测试更难查。
    #[test]
    fn report_after_switching_does_not_deadlock() {
        let base = std::env::temp_dir().join("aox-ws-report");
        std::fs::create_dir_all(&base).unwrap();
        let w = fixed(&base.to_string_lossy());
        let app = std::env::temp_dir();

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = tx.send(w.report(&app));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(text) => {
                handle.join().unwrap();
                assert!(text.contains("选择工作区"), "固定档位应报「选择工作区」：{text}");
                assert!(
                    text.contains(&normalize(&base).display().to_string()),
                    "正文里应当有根：{text}"
                );
            }
            Err(_) => panic!("report() 超过 5 秒没返回 —— 状态锁被重入了（死锁）"),
        }

        // `default` 档建成之后**档位仍然是「默认」**，不能因为根是现建的就说成「选择工作区」
        let app2 = std::env::temp_dir().join("aox-ws-report-default");
        let w2 = ws("default", None);
        w2.ensure_started(&app2).unwrap().expect("应当建出一块新地");
        let text = w2.report(&app2);
        assert!(text.contains("默认"), "{text}");
        assert!(!text.contains("选择工作区"), "default 档不该被报成「选择工作区」：{text}");
        let _ = std::fs::remove_dir_all(app2.join(WORKPLACE_DIR));
    }

    /// 回归：**整段源码不能被当成路径**。7a 实测到的真实缺陷 ——
    /// `create_plugin(source = "<2.2 KB 的插件源码>")` 里因为出现 `a/../b`，
    /// 整段源码被判成「含 `..` 分量的路径」。后果两边都坏：受限档下 `create_plugin`
    /// 直接被拒（**功能坏了**），full 档每次白烧约 1 ms（逐级 `canonicalize`）。
    #[test]
    fn multiline_source_is_not_treated_as_a_path() {
        let src = "import os\nsys.path.insert(0, os.path.join(d, \"..\"))\n# a/b/c\n".repeat(30);
        let args = serde_json::json!({
            "name": "demo",
            "source": src,
            "code": "x = a/b",
        });
        let hits = scan_paths(&args);
        assert!(
            !hits.iter().any(|(_, v)| v.contains("import os")),
            "整段源码被当成路径了：{:?}",
            hits.iter().map(|(k, v)| (k.clone(), v.len())).collect::<Vec<_>>()
        );
        assert!(hits.is_empty(), "这两个参数都不该命中：{hits:?}");

        // 但**键名权威**：同一个值挂在 `path` 键下，再长也照判（它真可能是条长路径）——
        // 钉住「闸门只装在靠值形态猜的那几条路上」，别哪天顺手把键名那条也闸了。
        let long_path = format!("C:\\{}", "x".repeat(300));
        let hits = scan_paths(&serde_json::json!({ "path": long_path }));
        assert_eq!(hits.len(), 1, "`path` 键下的长值是路径，不能因为长度被放过");
    }

    /// `is_unrestricted()` 是判定点**短路**的唯一依据（`full` 档下整条判定被跳过），
    /// 所以它必须与「`check` 会不会拒」严格一致：它说 `true` 的时候，任何路径都必须能过。
    /// 反过来说，哪天有人把 `Pending` 也划成 `true`，那就是「受限档位静默全放行」。
    #[test]
    fn is_unrestricted_agrees_with_check() {
        let full = ws("full", None);
        assert!(full.is_unrestricted(), "full 档必须报不受限");
        for p in ["relative.txt", "C:\\Windows\\System32", "/etc/passwd", "a/../../b"] {
            assert!(full.check(p).is_ok(), "full 档必须放行 {p:?}");
        }

        let base = std::env::temp_dir().join("aox-ws-unrestricted");
        std::fs::create_dir_all(&base).unwrap();
        let w = fixed(&base.to_string_lossy());
        assert!(!w.is_unrestricted(), "受限档不能报不受限 —— 否则判定被整个跳过");
        w.set_mode(Mode::Full, &base).unwrap();
        assert!(w.is_unrestricted(), "运行期切到 full 之后也必须短路");

        // `Pending`（default 档、还没发消息）是**全拒绝**，绝不能报不受限。
        assert!(!ws("default", None).is_unrestricted(), "Pending 是全拒绝，绝不能报不受限");
    }

    /// 新语义（2026-10-04）：**读不设限 / 写删受限**。
    ///
    /// 拆开对拍，免得哪天「顺手统一」又把两边改回同一档。四个状态 × 两种意图都要钉：
    /// 同一个越界路径，`check`（写）必须拒、`check_read`（读）必须放。
    #[test]
    fn read_is_never_restricted_but_write_is() {
        let base = std::env::temp_dir().join("aox-ws-rw");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let outside = base.parent().unwrap().join("aox-ws-rw-outside.txt");
        let outside = outside.to_string_lossy().into_owned();
        let inside = base.join("sub").join("a.txt").to_string_lossy().into_owned();

        // ① 固定档：越界路径 —— 写拒、读放；区内路径两种意图都放
        let w = fixed(&base.to_string_lossy());
        assert!(
            matches!(w.check(&outside), Err(Reject::Outside { .. })),
            "越界的**写**必须被拒"
        );
        assert!(w.check_read(&outside).is_ok(), "越界的**读**必须放行（新语义）");
        assert!(w.check(&inside).is_ok());
        assert!(w.check_read(&inside).is_ok());

        // ② 相对路径：写拒（宿主与插件 cwd 不同，那层歧义是写才有的风险）；
        //    读放 —— 读的后果只是「读到了别处的同名文件」，不值得为它拒掉一次正常读。
        assert!(matches!(w.check("config.toml"), Err(Reject::Relative { .. })));
        assert!(w.check_read("config.toml").is_ok(), "读不该因为相对路径被拒");

        // ③ `default` 还没开地（Pending）：写全拒、读照放 —— 否则「首条消息之前什么都读不到」
        let p = ws("default", None);
        assert!(matches!(p.check("C:\\Windows"), Err(Reject::NotStarted)));
        assert!(p.check_read("C:\\Windows").is_ok(), "地还没开也不该挡住读");

        // ④ full 档：两种意图都放（读的分支与 full 的写分支是两段代码，都要钉）
        let f = ws("full", None);
        assert!(f.check(&outside).is_ok());
        assert!(f.check_read(&outside).is_ok());
    }

    /// `snapshot()` 是**界面**唯一的取状态入口：一次取锁同时拿「档位」与「根」。
    ///
    /// 钉住两件事：
    /// ① 它与 `mode()` / `active_root()` 说的是同一件事 —— 否则会出现「按钮上写默认、
    ///    列表却在『选择工作区』上打勾」这种自相矛盾的界面；
    /// ② 它自己**不会重入状态锁**（重入的话这一句永远回不来 —— 与
    ///    `report_after_switching_does_not_deadlock` 同一个坑）。
    ///
    /// 尤其是 `default` 档建成之后：档位必须**仍然是 `Default`**，只是根从 `None`
    /// 变成了那条目录 —— 这正是 `snapshot()` 把「档位与根的映射」只写一遍的意义。
    #[test]
    fn snapshot_agrees_with_mode_and_root() {
        // full：档位 Full、根 None（不是「还没定」，是「不受限」）
        let full = ws("full", None);
        assert_eq!(full.snapshot(), (Mode::Full, None));
        assert_eq!(full.snapshot().0, full.mode());

        // default 建地之前：档位 Default、根 None（**不是**不受限）
        let app = std::env::temp_dir().join("aox-ws-snapshot");
        let d = ws("default", None);
        assert_eq!(d.snapshot(), (Mode::Default, None));

        // default 建地之后：档位仍是 Default，但根有了
        let dir = d.ensure_started(&app).unwrap().expect("应当建出一块新地");
        let (m, r) = d.snapshot();
        assert_eq!(m, Mode::Default, "default 档建成之后档位不该变成「选择工作区」");
        assert_eq!(r.as_deref(), Some(dir.as_path()));
        assert_eq!(d.mode(), m, "snapshot 与 mode 必须一致");
        assert_eq!(d.active_root(), r, "snapshot 与 active_root 必须一致");
        let _ = std::fs::remove_dir_all(app.join(WORKPLACE_DIR));

        // fixed：档位带路径、根是它自己
        let base = std::env::temp_dir().join("aox-ws-snapshot-fixed");
        std::fs::create_dir_all(&base).unwrap();
        let f = fixed(&base.to_string_lossy());
        let (m, r) = f.snapshot();
        assert!(matches!(m, Mode::Fixed(_)), "{m:?}");
        assert_eq!(r, Some(normalize(&base)));
        assert_eq!(f.mode(), m);
        assert_eq!(f.active_root(), r);
    }

    /// 档位的判据是**运行期怎么来的**，不是配置里写的什么（2026-10-04 修的 bug）。
    ///
    /// 实机现象：配置写 `mode = "default"`，运行期又挑了一块固定的地 ——
    /// `set_mode` 报「已切到「选择工作区」：X」，紧接着 `snapshot()` 报「档位：默认」。
    /// 同一个动作两种说法，GUI 上就是「按钮写着默认、打勾也在默认上，而写入去了 X」。
    #[test]
    fn snapshot_follows_the_runtime_origin_not_the_config() {
        let base = std::env::temp_dir().join("aox-ws-origin");
        std::fs::create_dir_all(base.join("picked")).unwrap();

        // 配置 `default` + 运行期挑了一块固定的地 ⇒ 必须报「选择工作区」
        let w = ws("default", None);
        let d = base.join("picked");
        w.set_mode(Mode::Fixed(d.clone()), &base).unwrap();
        let (m, r) = w.snapshot();
        assert!(matches!(m, Mode::Fixed(_)), "运行期点名挑的地要报「选择工作区」，实际 {m:?}");
        assert_eq!(r, Some(normalize(&d)));
        assert_eq!(w.persisted().mode, "fixed", "存档里也要是 fixed，否则下回打开又变回默认");

        // 而 `default` 档现建的地仍然报「默认」（picked=false）
        let w2 = ws("default", None);
        let app2 = std::env::temp_dir().join("aox-ws-origin-default");
        w2.ensure_started(&app2).unwrap().expect("应当建出一块新地");
        assert_eq!(w2.snapshot().0, Mode::Default);
        assert_eq!(w2.persisted().mode, "default");
        let _ = std::fs::remove_dir_all(app2.join(WORKPLACE_DIR));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 新建对话**一律回到「默认」档**（用户 2026-10-04 定的）。
    ///
    /// 老实现只在 `cfg.mode == "default"` 时重置，于是「上一段对话挑了某个目录，
    /// 点 + 之后新对话还在那个目录里」。现在无论配置写什么、上一段切到哪一档，
    /// 翻页后一律是「默认」且根**尚未定**（等首条消息现建一块新地）。
    #[test]
    fn new_conversation_always_returns_to_default() {
        let base = std::env::temp_dir().join("aox-ws-newconv");
        std::fs::create_dir_all(base.join("picked")).unwrap();
        let app = std::env::temp_dir().join("aox-ws-newconv-app");

        // 配置写 `fixed` 也一样：翻页后不再是那块固定的地
        let w = fixed(&base.join("picked").to_string_lossy());
        assert!(matches!(w.snapshot().0, Mode::Fixed(_)));
        w.reset_for_new_conversation();
        assert_eq!(w.snapshot(), (Mode::Default, None), "新对话应当回到「默认」且根未定");

        // 切到「完全访问」之后翻页，同样回到「默认」（不是继承 full）
        w.set_mode(Mode::Full, &base).unwrap();
        assert_eq!(w.snapshot().0, Mode::Full);
        w.reset_for_new_conversation();
        assert_eq!(w.snapshot(), (Mode::Default, None));

        // 「根未定」的语义要真的成立：下一条消息会现建一块**新**地（在 workplace/ 下）
        let built = w.ensure_started(&app).unwrap().expect("应当现建一块新地");
        assert!(
            built.starts_with(normalize(&app.join(WORKPLACE_DIR))),
            "新对话的地应当在 workplace/ 下，实际 {}",
            built.display()
        );
        let _ = std::fs::remove_dir_all(app.join(WORKPLACE_DIR));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `persisted()` 与 `restore()` 必须互为逆运算 —— 打开旧对话就是把存下来的那份放回去。
    ///
    /// 关键几点：`default` 档带根的那一支**根要沿用**（否则「同一段对话每次进去都是
    /// 不同的工作区」）；`own` 也要进存档、并且在 `full` / `fixed` 档下**留着**
    /// （否则「切走再切回默认」会另建一块）。
    #[test]
    fn persisted_and_restore_round_trip() {
        let base = std::env::temp_dir().join("aox-ws-persist");
        std::fs::create_dir_all(base.join("picked")).unwrap();
        let app = std::env::temp_dir().join("aox-ws-persist-app");

        // ① default：地是现建的 → 存档里 mode=default，root 与 own 都是那块地
        let w = ws("default", None);
        let dir = w.ensure_started(&app).unwrap().expect("应当建出一块新地");
        let saved = w.persisted();
        assert_eq!(saved.mode, "default");
        assert_eq!(saved.root.as_ref(), Some(&normalize(&dir)));
        assert_eq!(saved.own.as_ref(), Some(&normalize(&dir)), "自建地也要进存档");
        let fresh = ws("full", None); // 故意从别的档起 —— 还原要能压过它
        fresh.restore(&saved);
        assert_eq!(fresh.snapshot(), (Mode::Default, Some(normalize(&dir))));
        assert_eq!(fresh.persisted(), saved, "往返一圈必须一模一样");

        // ② fixed：点名挑的地，`own` 为空
        let w = fixed(&base.join("picked").to_string_lossy());
        let saved = w.persisted();
        assert_eq!(saved.mode, "fixed");
        assert_eq!(saved.own, None);
        let fresh = ws("full", None);
        fresh.restore(&saved);
        let (m, r) = fresh.snapshot();
        assert!(matches!(m, Mode::Fixed(_)), "{m:?}");
        assert_eq!(r, saved.root);

        // ③ full：当前档位没有根，但**自建地要留着** —— 切回默认时靠它找回原来那块
        let own = normalize(&app.join(WORKPLACE_DIR).join("20260101000000"));
        let saved = Record { mode: "full".into(), root: None, own: Some(own.clone()) };
        let fresh = fixed(&base.join("picked").to_string_lossy());
        fresh.restore(&saved);
        assert_eq!(fresh.snapshot(), (Mode::Full, None));
        assert_eq!(fresh.persisted().own.as_ref(), Some(&own), "full 档下也不许丢自建地");
        fresh.set_mode(Mode::Default, &base).unwrap();
        assert_eq!(fresh.snapshot(), (Mode::Default, Some(own)), "切回默认应当回到存档里那块");

        // ④ 认不出的档位名（或 default 没给根）⇒ 回「还没定」，让首条消息现建
        let fresh = ws("full", None);
        fresh.restore(&Record { mode: "nonsense".into(), root: None, own: None });
        assert_eq!(fresh.snapshot(), (Mode::Default, None));

        let _ = std::fs::remove_dir_all(app.join(WORKPLACE_DIR));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// **切走再切回「默认」要回到本对话原来那块地，不再新建。**
    ///
    /// 用户 2026-10-05 报的：「新建对话 → 说第一句（建了一块地）→ 切完全访问 → 切回默认」
    /// 又开一块新的，于是 `workplace/` 被空目录一块块堆满（「无穷无尽的工作区累加」）。
    #[test]
    fn switching_back_to_default_reuses_the_conversation_own_dir() {
        let app = std::env::temp_dir().join("aox-ws-own");
        let base = std::env::temp_dir().join("aox-ws-own-base");
        std::fs::create_dir_all(&base).unwrap();

        let w = ws("default", None);
        let first = w.ensure_started(&app).unwrap().expect("首条消息应当建出一块地");

        // 来回切几次：每次都必须是同一块，且不再建目录
        for _ in 0..3 {
            w.set_mode(Mode::Full, &base).unwrap();
            assert_eq!(w.snapshot(), (Mode::Full, None));
            let msg = w.set_mode(Mode::Default, &base).unwrap();
            assert_eq!(
                w.snapshot(),
                (Mode::Default, Some(normalize(&first))),
                "切回默认应当回到本对话那块地"
            );
            assert!(msg.contains("仍在"), "提示语该说「仍在原来那块」，实际：{msg}");
            // 首条消息那一步不该再建（`started` 仍为真 ⇒ 返回 None）
            assert!(w.ensure_started(&app).unwrap().is_none(), "不该再建第二块地");
        }

        let made = std::fs::read_dir(app.join(WORKPLACE_DIR)).map(|d| d.count()).unwrap_or(0);
        assert_eq!(made, 1, "来回切档不该攒出多个目录，实际 {made} 个");

        // 只有**新对话**才另开一块：`reset_for_new_conversation` 会把自建地清掉
        w.reset_for_new_conversation();
        assert_eq!(w.snapshot(), (Mode::Default, None));
        let second = w.ensure_started(&app).unwrap().expect("新对话应当另建一块");
        assert_ne!(second, first, "新对话本来就该是另一块地");

        let _ = std::fs::remove_dir_all(app.join(WORKPLACE_DIR));
        let _ = std::fs::remove_dir_all(&base);
    }
}
