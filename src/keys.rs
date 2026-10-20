//! API key store: keys are created and revoked through the admin API and kept in a
//! JSON file. Only a SHA-256 hash of each key is stored; the full key is returned
//! once, at creation.

use std::{
    path::PathBuf,
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub struct KeyRecord {
    pub id: String,
    pub name: String,
    /// First characters of the key, so people can tell keys apart.
    pub prefix: String,
    hash: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub request_count: u64,
    pub disabled: bool,
}

impl KeyRecord {
    /// The public view: everything except the hash.
    pub fn view(&self) -> Value {
        json!({
            "type": "api_key",
            "id": self.id,
            "name": self.name,
            "partial_key_hint": format!("{}…", self.prefix),
            "status": if self.disabled { "disabled" } else { "active" },
            "created_at": self.created_at,
            "last_used_at": self.last_used_at,
            "request_count": self.request_count,
        })
    }
}

pub struct KeyStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    keys: Vec<KeyRecord>,
    /// Usage counters changed since the last save.
    dirty: bool,
}

fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 244 bits of OS randomness (two v4 UUIDs) as hex.
fn random_hex() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

impl KeyStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let keys = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Ok(Self { path, inner: Mutex::new(Inner { keys, dirty: false }) })
    }

    pub fn create(&self, name: String) -> Result<(KeyRecord, String), String> {
        let secret = format!("sk-gw-{}", random_hex());
        let record = KeyRecord {
            id: format!("key_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]),
            name,
            prefix: secret[..14].to_string(),
            hash: hash(&secret),
            created_at: now(),
            last_used_at: None,
            request_count: 0,
            disabled: false,
        };
        let mut inner = self.inner.lock().unwrap();
        inner.keys.push(record.clone());
        self.save(&mut inner)?;
        Ok((record, secret))
    }

    pub fn list(&self) -> Vec<KeyRecord> {
        self.inner.lock().unwrap().keys.clone()
    }

    pub fn get(&self, id: &str) -> Option<KeyRecord> {
        self.inner.lock().unwrap().keys.iter().find(|k| k.id == id).cloned()
    }

    pub fn update(&self, id: &str, name: Option<String>, disabled: Option<bool>) -> Result<Option<KeyRecord>, String> {
        let mut inner = self.inner.lock().unwrap();
        let Some(key) = inner.keys.iter_mut().find(|k| k.id == id) else { return Ok(None) };
        if let Some(name) = name {
            key.name = name;
        }
        if let Some(disabled) = disabled {
            key.disabled = disabled;
        }
        let updated = key.clone();
        self.save(&mut inner)?;
        Ok(Some(updated))
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.keys.len();
        inner.keys.retain(|k| k.id != id);
        if inner.keys.len() == before {
            return Ok(false);
        }
        self.save(&mut inner)?;
        Ok(true)
    }

    /// Check a presented key; on success record the use and return the key's name.
    pub fn authenticate(&self, secret: &str) -> Option<String> {
        let h = hash(secret);
        let mut inner = self.inner.lock().unwrap();
        let key = inner.keys.iter_mut().find(|k| k.hash == h && !k.disabled)?;
        key.request_count += 1;
        key.last_used_at = Some(now());
        let name = key.name.clone();
        inner.dirty = true;
        Some(name)
    }

    /// Save usage counters if they changed (called periodically).
    pub fn flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.dirty {
            if let Err(e) = self.save(&mut inner) {
                tracing::warn!("saving key usage: {e}");
            }
        }
    }

    /// Write atomically (temp file + rename), readable only by this user.
    fn save(&self, inner: &mut Inner) -> Result<(), String> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&inner.keys).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        inner.dirty = false;
        Ok(())
    }
}
