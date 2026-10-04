//! 授权策略（Policy 缝）。
//!
//! 把「这次工具调用该不该放行」从写死的三档判断，抽成可替换的策略对象：
//! `Runtime` 只持一个 `Arc<dyn Policy>`，不再知道策略是怎么算出来的。
//!
//! 阶段 3 只做「抽缝 + 原样搬运」：`ConfigPolicy` 就是原来的 `AuthPolicy`，
//! 三档语义（auto / ask / deny）与 override 优先级（**工具名 > 插件名 > 类别默认**）
//! 一行没改。将来要加别的策略来源（比如策略文件、插件自带策略），是实现一个新的
//! `Policy` 而不是改这里。

use crate::config::AuthConfig;
use crate::tool::ToolKind;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthLevel {
    /// 直接放行
    Auto,
    /// 询问用户
    Ask,
    /// 直接拒绝
    Deny,
}

impl AuthLevel {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "allow" | "always" => Self::Auto,
            "deny" | "never" => Self::Deny,
            _ => Self::Ask,
        }
    }
}

/// 授权策略：回答「这个工具调用要 auto、ask 还是 deny」。
///
/// `Send + Sync`：`Runtime` 被 agent 线程与 GUI 主线程共享，策略对象必须能跨线程持有。
pub trait Policy: Send + Sync {
    /// `caps`：这次调用**申请的能力**。
    ///
    /// 阶段 3 立好了参数位，阶段 4 仍传空切片（那两项配置都不是「能力申请」），
    /// **阶段 6 · 6b 起真的填上了**：组件在 `[contributes] caps` 里声明要什么能力，
    /// 宿主把该插件工具的申请原样传进这里 —— 于是「组件申请能力」直接落在同一套
    /// 授权判定里（原则 P7），没有第二条判定路径。
    fn level_for(&self, kind: &ToolKind, tool_name: &str, caps: &[String]) -> AuthLevel;
}

/// 由 `config.toml [auth]` 决定的策略：overrides 优先（按工具名，其次按插件名），
/// 否则按工具类别取默认值。
///
/// override 的键有两种写法（阶段 4 起）：
/// - **完整名字**（`shell_exec`）—— 精确命中；
/// - **尾随 `*` 的前缀**（`fs_*`）—— 命中所有以该前缀开头的名字。
///
/// 通配只看前缀、不认分隔符：`fs*` 会连 `fsx` 一起命中，所以想只圈住一组工具，
/// 记得把下划线写进去（`fs_*`）。单独一个 `*` 是「兜底一切」。
pub struct ConfigPolicy {
    meta: AuthLevel,
    plugin: AuthLevel,
    overrides: HashMap<String, AuthLevel>,
    /// 带 `*` 的那些 override（存的是去掉 `*` 的前缀），按前缀**从长到短**排好 ——
    /// 越具体的先命中：`fs_read_*` 必须压过 `fs_*`。
    wildcards: Vec<(String, AuthLevel)>,
}

impl ConfigPolicy {
    pub fn from_config(cfg: &AuthConfig) -> Self {
        let mut overrides = HashMap::new();
        let mut wildcards: Vec<(String, AuthLevel)> = Vec::new();
        for (k, v) in &cfg.overrides {
            let level = AuthLevel::parse(v);
            let key = k.trim();
            match key.strip_suffix('*') {
                Some(prefix) => wildcards.push((prefix.to_string(), level)),
                None => {
                    overrides.insert(key.to_string(), level);
                }
            }
        }
        // 排序有两个作用：长前缀优先（更具体的规则赢），以及让判定与 HashMap 的随机
        // 遍历顺序无关 —— 同一份配置必须每次给出同一个答案。
        wildcards.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Self {
            meta: AuthLevel::parse(&cfg.meta_tools),
            plugin: AuthLevel::parse(&cfg.plugin_tools),
            overrides,
            wildcards,
        }
    }

    /// 查一个名字（工具名或插件名）：**先精确、再通配**。
    fn lookup(&self, name: &str) -> Option<AuthLevel> {
        if let Some(l) = self.overrides.get(name) {
            return Some(*l);
        }
        self.wildcards
            .iter()
            .find(|(prefix, _)| name.starts_with(prefix.as_str()))
            .map(|(_, l)| *l)
    }
}

impl Policy for ConfigPolicy {
    fn level_for(&self, kind: &ToolKind, tool_name: &str, caps: &[String]) -> AuthLevel {
        // ① 工具名（精确 → 通配）
        if let Some(l) = self.lookup(tool_name) {
            return l;
        }
        // ② 插件名（精确 → 通配）—— 同样属于「用户明确写下的表态」，压在能力位之上
        let plugin_hit = match kind {
            ToolKind::Plugin(plugin_name) => self.lookup(plugin_name),
            ToolKind::Meta => None,
        };
        if let Some(l) = plugin_hit {
            return l;
        }
        // ③ 能力申请（阶段 6 · 6b 首次填值，原则 P7）：组件在 `[contributes] caps` 里
        //    声明「我要什么能力」，这类调用**一律问一次** —— 不许静默搭类别默认的便车。
        //    例：`[auth] plugin_tools = "auto"` 的意思是「常规插件随便跑」，不该顺势变成
        //    「顺手把写文件的权利也批了」。位置刻意放在 override 之后：用户手写的
        //    override 是明确表态，比组件的申请更该说话。
        //
        //    只对**插件工具**生效：能力申请只可能来自组件，元工具是宿主自己的、
        //    不替任何人申请。多这一道判断是为了「传了就生效」不会意外波及元工具。
        if matches!(kind, ToolKind::Plugin(_)) && !caps.is_empty() {
            return AuthLevel::Ask;
        }
        // ④ 都没有才退到类别默认
        match kind {
            ToolKind::Meta => self.meta,
            ToolKind::Plugin(_) => self.plugin,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AuthConfig {
        AuthConfig {
            meta_tools: "ask".into(),
            plugin_tools: "deny".into(),
            overrides: [
                ("create_plugin".to_string(), "auto".to_string()),
                ("myplugin".to_string(), "ask".to_string()),
            ]
            .into_iter()
            .collect(),
        }
    }

    /// 抽缝是纯重构 —— 这条把原来的优先级钉死：工具名 override > 插件名 override > 类别默认。
    #[test]
    fn override_beats_category_defaults() {
        let p = ConfigPolicy::from_config(&cfg());
        // ① 按工具名 override 最优先，连类别默认都盖掉
        assert_eq!(p.level_for(&ToolKind::Meta, "create_plugin", &[]), AuthLevel::Auto);
        // ② 其次按插件名 override
        assert_eq!(
            p.level_for(&ToolKind::Plugin("myplugin".into()), "myplugin_fs", &[]),
            AuthLevel::Ask
        );
        // ③ 没有 override 时退回类别默认
        assert_eq!(p.level_for(&ToolKind::Meta, "list_plugins", &[]), AuthLevel::Ask);
        assert_eq!(p.level_for(&ToolKind::Plugin("other".into()), "other_do", &[]), AuthLevel::Deny);
    }

    /// 工具名 override 不该被插件名 override 抢走 —— 两条查询的顺序不能反。
    #[test]
    fn tool_name_wins_over_plugin_name() {
        let mut c = cfg();
        c.overrides.insert("myplugin".into(), "deny".into());
        c.overrides.insert("myplugin_fs".into(), "auto".into());
        let p = ConfigPolicy::from_config(&c);
        assert_eq!(
            p.level_for(&ToolKind::Plugin("myplugin".into()), "myplugin_fs", &[]),
            AuthLevel::Auto,
            "同名工具与同名插件同时有 override 时，以工具为准"
        );
    }

    /// 三档字符串的别名与大小写：改了会让用户写在 config 里的值悄悄变成 ask。
    #[test]
    fn level_parse_accepts_aliases_case_insensitively() {
        for s in ["auto", "AUTO", " allow ", "always"] {
            assert_eq!(AuthLevel::parse(s), AuthLevel::Auto, "{s}");
        }
        for s in ["deny", "DENY", "never"] {
            assert_eq!(AuthLevel::parse(s), AuthLevel::Deny, "{s}");
        }
        for s in ["ask", "whatever", ""] {
            assert_eq!(AuthLevel::parse(s), AuthLevel::Ask, "{s:?}");
        }
    }

    fn cfg_with_overrides(pairs: &[(&str, &str)]) -> AuthConfig {
        AuthConfig {
            meta_tools: "ask".into(),
            plugin_tools: "auto".into(),
            overrides: pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
        }
    }

    /// 尾随 `*` 的前缀通配：一次挡住「一类工具」。
    #[test]
    fn wildcard_override_matches_by_prefix() {
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("fs_*", "deny")]));
        assert_eq!(p.level_for(&ToolKind::Plugin("fs".into()), "fs_read_file", &[]), AuthLevel::Deny);
        assert_eq!(p.level_for(&ToolKind::Plugin("fs".into()), "fs_write_file", &[]), AuthLevel::Deny);
        // 没被通配圈住的仍走原来的优先级（插件默认 auto）
        assert_eq!(p.level_for(&ToolKind::Plugin("net".into()), "net_fetch", &[]), AuthLevel::Auto);
    }

    /// 三层优先级在通配加入之后**一个字都没变**：
    /// 精确工具名 > 更长的前缀 > 更短的前缀 > 插件默认。
    #[test]
    fn exact_key_beats_wildcard_and_longer_prefix_wins() {
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[
            ("fs_*", "deny"),
            ("fs_read_*", "auto"),
            ("fs_read_file", "ask"),
        ]));
        let kind = ToolKind::Plugin("fs".into());
        // ① 精确键最优先，连更长的前缀都盖得住
        assert_eq!(p.level_for(&kind, "fs_read_file", &[]), AuthLevel::Ask);
        // ② 没有精确键时，长前缀（更具体）赢
        assert_eq!(p.level_for(&kind, "fs_read_dir", &[]), AuthLevel::Auto);
        // ③ 都不匹配才退到短前缀
        assert_eq!(p.level_for(&kind, "fs_write_file", &[]), AuthLevel::Deny);
        // ④ 插件名同样吃通配 —— `myplugin*` 命中插件名 myplugin
        let p2 = ConfigPolicy::from_config(&cfg_with_overrides(&[("myplugin*", "deny")]));
        assert_eq!(
            p2.level_for(&ToolKind::Plugin("myplugin".into()), "unrelated", &[]),
            AuthLevel::Deny
        );
    }

    /// 单独一个 `*` = 兜底一切 —— 既然写得出来，就该按字面意思生效。
    #[test]
    fn bare_star_catches_everything() {
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("*", "deny")]));
        assert_eq!(p.level_for(&ToolKind::Meta, "list_plugins", &[]), AuthLevel::Deny);
        assert_eq!(
            p.level_for(&ToolKind::Plugin("any".into()), "any_tool", &[]),
            AuthLevel::Deny
        );
    }

    /// 通配是**前缀**匹配，不认分隔符 —— 这条把语义钉死，免得以后有人以为它有路径感知。
    #[test]
    fn wildcard_is_a_plain_prefix() {
        // 没有 `*` 就是精确匹配：键 `fs` 只命中叫 `fs` 的东西，不命中 fs_read_file
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("fs", "deny")]));
        assert_eq!(
            p.level_for(&ToolKind::Plugin("other".into()), "fs_read_file", &[]),
            AuthLevel::Auto
        );

        // 写成 `fs*` 才按前缀命中 —— 连 `fsx_do` 一起命中（这就是「只看前缀」的含义）
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("fs*", "deny")]));
        assert_eq!(p.level_for(&ToolKind::Plugin("x".into()), "fs_read_file", &[]), AuthLevel::Deny);
        assert_eq!(p.level_for(&ToolKind::Plugin("x".into()), "fsx_do", &[]), AuthLevel::Deny);
    }

    /// 能力申请（阶段 6 · 6b 首次填值）：声明了 caps 的组件，它的工具调用一律走问询。
    #[test]
    fn declared_caps_force_ask_over_plugin_default() {
        // 插件类别默认是 auto（见 cfg_with_overrides）—— 常规插件不必问
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[]));
        let kind = ToolKind::Plugin("comp".into());
        assert_eq!(p.level_for(&kind, "comp_do", &[]), AuthLevel::Auto);
        // 但组件一旦申请能力，就必须让人点一次头
        assert_eq!(p.level_for(&kind, "comp_do", &["fs_write".to_string()]), AuthLevel::Ask);
    }

    /// 用户手写的 override 压过能力申请 —— 那是明确表态，比组件的申请更该说话。
    #[test]
    fn explicit_override_beats_declared_caps() {
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("comp", "auto"), ("comp_do", "deny")]));
        let kind = ToolKind::Plugin("comp".into());
        let caps = ["fs_write".to_string()];
        assert_eq!(p.level_for(&kind, "comp_do", &caps), AuthLevel::Deny, "工具名 override 最优先");
        assert_eq!(p.level_for(&kind, "comp_other", &caps), AuthLevel::Auto, "插件名 override 次之");
    }

    /// 能力位**只影响插件工具那条路**：元工具是宿主自己的，不替谁申请能力、
    /// 也不该被误伤（这条钉的是「传了就生效」不会意外波及元工具）。
    #[test]
    fn caps_never_apply_to_meta_tools() {
        // meta_tools = deny：如果 caps 判断漏到这条路上，结果会变成 Ask
        let p = ConfigPolicy::from_config(&cfg_with_overrides(&[("*", "deny")]));
        assert_eq!(
            p.level_for(&ToolKind::Meta, "list_plugins", &["fs_write".to_string()]),
            AuthLevel::Deny,
            "兜底 override 命中元工具，能力位不该把它改成 ask"
        );
    }
}
