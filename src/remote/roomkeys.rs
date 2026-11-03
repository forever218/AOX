//! 房间口令 → 密钥，按房间保存在 `.aox/room-keys.json`。
//!
//! 存的是**口令**而不是派生的密钥：密钥的盐是房间 id，口令才是人能重新输入的那份东西。
//! 文件在 Unix 下 0600；Windows 没有等价物，这一点写在 README 里，不假装它是保险箱。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use super::crypto;

pub struct RoomKeys {
    path: PathBuf,
    /// room_id → 口令
    phrases: Mutex<HashMap<String, String>>,
}

impl RoomKeys {
    pub fn load(root: &Path) -> Self {
        let path = root.join(".aox").join("room-keys.json");
        let phrases = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<HashMap<String, String>>(&t).ok())
            .unwrap_or_default();
        Self { path, phrases: Mutex::new(phrases) }
    }

    pub fn get(&self, room: &str) -> Option<[u8; 32]> {
        let g = self.phrases.lock().expect("room keys");
        g.get(room).map(|p| crypto::derive_key(p, room))
    }

    pub fn has(&self, room: &str) -> bool {
        self.phrases.lock().expect("room keys").contains_key(room)
    }

    pub fn set(&self, room: &str, passphrase: &str) -> Result<()> {
        {
            let mut g = self.phrases.lock().expect("room keys");
            g.insert(room.to_string(), passphrase.to_string());
        }
        self.save()
    }

    pub fn clear(&self, room: &str) -> Result<bool> {
        let removed = {
            let mut g = self.phrases.lock().expect("room keys");
            g.remove(room).is_some()
        };
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        let snapshot = {
            let g = self.phrases.lock().expect("room keys");
            g.clone()
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&snapshot)?)
            .with_context(|| format!("写入 {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}
