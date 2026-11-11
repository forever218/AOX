//! All pairings saved locally on this machine.
//! v2 is a list: old pairings survive server switches; active picks which one serve uses.
//! Address is read-only (token-bound); switch by re-pairing. Tokens stay out of config.toml and are 0600 on Unix.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 2;

/// How to handle @-mentions from others, stored per pairing since trust varies by server/room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallPolicy {
    /// Only execute the owner's instructions; reject @-mentions from anyone else.
    Owner,
    /// Ask the owner to approve every request (the default).
    #[default]
    Ask,
    /// Let anyone use it without asking; only for fully trusted rooms.
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
    /// Server address entered at pairing time; never hard-coded since the distro belongs to no single server.
    pub server: String,
    pub device_id: String,
    pub token: String,
    pub agent_id: String,
    pub agent_handle: String,
    pub device_name: String,
    #[serde(default)]
    pub owner_id: String,
    /// Local nickname for this pairing, kept on the device and never uploaded.
    #[serde(default)]
    pub label: String,
    /// How to handle others' @-mentions; defaults to ask every time.
    #[serde(default)]
    pub call_policy: CallPolicy,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_connected_at: i64,
    /// Group-to-session map, one per group, resumed across restarts; /new clears one entry.
    #[serde(default)]
    pub rooms: std::collections::HashMap<String, RoomConv>,
}

impl RemoteProfile {
    /// The one-line title shown in the pairings list.
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

    /// Map https to wss and http to ws for LAN/dev servers, appending the /ws path.
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

/// Maps a group to a session id for resume across restarts; /new deletes the entry, a missing file starts fresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomConv {
    pub conversation: String,
    /// Room name, possibly empty for old pairings; used only for display.
    #[serde(default)]
    pub name: String,
    /// Timestamp of the last exchange in ms; currently only for diagnostics, not used for cleanup.
    #[serde(default)]
    pub updated_at: i64,
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
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };

        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;

        if value.get("pairings").is_some() {
            let book: Self = serde_json::from_value(value)
                .with_context(|| format!("{} is not a valid remote.json", path.display()))?;
            return Ok(book);
        }

        // Migrate the v1 single-object file into one list entry without losing credentials or re-pairing.
        let old: V1 = serde_json::from_value(value)
            .with_context(|| format!("{} is neither v2 nor a recognizable v1", path.display()))?;
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
            rooms: Default::default(),
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

    /// Find a pairing by id or by 1-based index, the latter to save typing.
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

    /// Overwrite the entry with the same id or append it, then make it active.
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
        anyhow::bail!("the server address cannot be empty");
    }
    let s = if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    let url: reqwest::Url = s.parse().with_context(|| format!("{s} is not a valid address"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("only http/https is supported");
    }
    Ok(s)
}
