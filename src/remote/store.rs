//! 本机保存的所有配对。
//!
//! **v2 是一个列表**（v1 是单条）。理由很实际：换服务器、家里公司各一台，都需要"留着旧的不删"，
//! 而每次重配都要重新找手机要配对码。列表里用 `active` 指定 `serve` 连哪一条。
//!
//! 服务端地址是**只读**的：token 与端点绑定，改地址等于凭证失效。想换服务器就删了重配，
//! 只留一条路径，用户不会在"改了地址却连不上"里打转。
//!
//! 令牌不进 `config.toml`——那个文件是给人看和贴进 issue 的；这里是机器状态，Unix 下 0600。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 2;

/// 别人 `@` 我的机器时怎么办。存在配对项里——不同服务器/房间的信任程度不一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallPolicy {
    /// 只执行主人的指令，别人的 `@` 直接回绝。
    Owner,
    /// 每次都要主人点同意（默认）。
    #[default]
    Ask,
    /// 谁都能用，不再询问。只适合完全信任的房间。
    Open,
}

impl CallPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Ask => "ask",
            Self::Open => "open",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "owner" => Some(Self::Owner),
            "ask" => Some(Self::Ask),
            "open" => Some(Self::Open),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProfile {
    pub id: String,
    /// 服务端地址，用户配对时输入。**不硬编码**：分发版不属于任何一台服务器。
    pub server: String,
    pub device_id: String,
    pub token: String,
    pub agent_id: String,
    pub agent_handle: String,
    pub device_name: String,
    #[serde(default)]
    pub owner_id: String,
    /// 本机给这条配对起的名字（"公司服务器"）。只在本地，不上传。
    #[serde(default)]
    pub label: String,
    /// 别人 `@` 我的机器时怎么办。缺省 ask：每次都问。
    #[serde(default)]
    pub call_policy: CallPolicy,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_connected_at: i64,
}

impl RemoteProfile {
    /// 列表里显示的那一行标题。
    pub fn title(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.server
        } else {
            &self.label
        }
    }

    pub fn path(root: &Path) -> PathBuf {
        Book::path(root)
    }

    /// `https://host` → `wss://host/ws`；`http://` → `ws://`（只用于局域网/开发服务器）。
    pub fn ws_url(&self) -> String {
        let base = self.server.trim_end_matches('/');
        let ws = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            base.to_string()
        };
        format!("{ws}/ws")
    }

    pub fn api(&self, path: &str) -> String {
        format!("{}{}", self.server.trim_end_matches('/'), path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Book {
    pub version: u32,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub pairings: Vec<RemoteProfile>,
}

#[derive(Deserialize)]
struct V1 {
    server: String,
    device_id: String,
    token: String,
    agent_id: String,
    agent_handle: String,
    device_name: String,
    #[serde(default)]
    owner_id: String,
}

impl Book {
    pub fn path(root: &Path) -> PathBuf {
        root.join(".aox").join("remote.json")
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self { version: VERSION, active: None, pairings: Vec::new() })
            }
            Err(e) => return Err(e).with_context(|| format!("读取 {}", path.display())),
        };

        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("{} 不是有效的 JSON", path.display()))?;

        if value.get("pairings").is_some() {
            let book: Self = serde_json::from_value(value)
                .with_context(|| format!("{} 不是有效的 remote.json", path.display()))?;
            return Ok(book);
        }

        // v1：单条对象 → 迁移成一条列表项，不丢凭证、不需要重新配对。
        let old: V1 = serde_json::from_value(value)
            .with_context(|| format!("{} 既不是 v2 也不是可识别的 v1", path.display()))?;
        let id = format!("p{}", aoxchat_proto::now_ms());
        let profile = RemoteProfile {
            id: id.clone(),
            server: old.server,
            device_id: old.device_id,
            token: old.token,
            agent_id: old.agent_id,
            agent_handle: old.agent_handle,
            device_name: old.device_name,
            owner_id: old.owner_id,
            label: String::new(),
            call_policy: CallPolicy::default(),
            created_at: aoxchat_proto::now_ms(),
            last_connected_at: 0,
        };
        let book = Self { version: VERSION, active: Some(id), pairings: vec![profile] };
        let _ = book.save(root); // 迁移失败不致命，下次启动再试
        Ok(book)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn active(&self) -> Option<&RemoteProfile> {
        let id = self.active.as_ref()?;
        self.pairings.iter().find(|p| &p.id == id)
    }

    pub fn active_mut(&mut self) -> Option<&mut RemoteProfile> {
        let id = self.active.clone()?;
        self.pairings.iter_mut().find(|p| p.id == id)
    }

    /// 按 id 或 1 起始的序号找一条。序号是为了让人少打字。
    pub fn find(&self, needle: &str) -> Option<usize> {
        let needle = needle.trim();
        if let Ok(n) = needle.parse::<usize>() {
            if n >= 1 && n <= self.pairings.len() {
                return Some(n - 1);
            }
        }
        self.pairings.iter().position(|p| p.id == needle)
    }

    pub fn select(&mut self, needle: &str) -> bool {
        match self.find(needle) {
            Some(i) => {
                self.active = Some(self.pairings[i].id.clone());
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, needle: &str) -> bool {
        let Some(i) = self.find(needle) else { return false };
        let removed = self.pairings.remove(i);
        if self.active.as_deref() == Some(removed.id.as_str()) {
            self.active = self.pairings.first().map(|p| p.id.clone());
        }
        true
    }

    pub fn rename(&mut self, needle: &str, label: &str) -> bool {
        match self.find(needle) {
            Some(i) => {
                self.pairings[i].label = label.trim().to_string();
                true
            }
            None => false,
        }
    }

    /// 同 id 覆盖，否则追加，并设为当前。
    pub fn upsert(&mut self, profile: RemoteProfile) {
        if let Some(slot) = self.pairings.iter_mut().find(|p| p.id == profile.id) {
            *slot = profile.clone();
        } else {
            self.pairings.push(profile.clone());
        }
        self.active = Some(profile.id);
        self.version = VERSION;
    }
}

/// Accept what people actually type: `chat.example.com`, `https://chat.example.com/`, with spaces.
pub fn normalize_server(input: &str) -> Result<String> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        anyhow::bail!("服务端地址不能为空");
    }
    let s = if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    let url: reqwest::Url = s.parse().with_context(|| format!("{s} 不是有效地址"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("只支持 http/https");
    }
    Ok(s)
}
