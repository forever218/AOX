//! Map room passphrases to keys, stored per room in .aox/room-keys.json.
//! Store the passphrase, not the key: keys are salted by room id, and only the passphrase can be re-typed.
//! File is 0600 on Unix; Windows has no equivalent, so this is not treated as a secure vault.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use super::crypto;

pub struct RoomKeys {
    path: PathBuf,
    /// Maps room_id to its passphrase.
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
            .with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}
