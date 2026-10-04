use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;

fn yes() -> bool {
    true
}

fn default_branch() -> String {
    "warszawa".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscriber {
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub username: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default = "yes")]
    pub active: bool,
    #[serde(default)]
    pub created_at: i64,
}

/// Tiny JSON-file store: subscribers + discovered centre ids.
/// The subscribers file keeps the same shape as the old Python bot.
pub struct Store {
    subs_path: PathBuf,
    centers_path: PathBuf,
    subs: Mutex<HashMap<i64, Subscriber>>,
    centers: Mutex<HashMap<String, u32>>,
}

impl Store {
    pub fn load(data_dir: &PathBuf) -> Self {
        let subs_path = data_dir.join("subscribers.json");
        let centers_path = data_dir.join("centers.json");
        let _ = fs::create_dir_all(data_dir);

        let subs = read_json::<HashMap<String, Subscriber>>(&subs_path)
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, mut s)| {
                        s.user_id = k.parse().ok()?;
                        Some((s.user_id, s))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let centers = read_json(&centers_path).unwrap_or_default();

        Store { subs_path, centers_path, subs: Mutex::new(subs), centers: Mutex::new(centers) }
    }

    pub fn touch(&self, user_id: i64, username: Option<&str>) -> Subscriber {
        let mut map = self.subs.lock().unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let sub = map.entry(user_id).or_insert_with(|| Subscriber {
            user_id,
            username: String::new(),
            branch: default_branch(),
            active: true,
            created_at: now,
        });
        if let Some(u) = username {
            if !u.is_empty() {
                sub.username = u.to_string();
            }
        }
        let out = sub.clone();
        self.save_subs(&map);
        out
    }

    pub fn set_active(&self, user_id: i64, active: bool) {
        let sub = self.touch(user_id, None);
        let mut map = self.subs.lock().unwrap();
        if let Some(s) = map.get_mut(&sub.user_id) {
            s.active = active;
        }
        self.save_subs(&map);
    }

    pub fn set_branch(&self, user_id: i64, branch: &str) {
        self.touch(user_id, None);
        let mut map = self.subs.lock().unwrap();
        if let Some(s) = map.get_mut(&user_id) {
            s.branch = branch.to_string();
        }
        self.save_subs(&map);
    }

    fn get(&self, user_id: i64) -> Option<Subscriber> {
        self.subs.lock().unwrap().get(&user_id).cloned()
    }

    pub fn branch_of(&self, user_id: i64) -> String {
        self.get(user_id).map(|s| s.branch).unwrap_or_else(default_branch)
    }

    pub fn is_active(&self, user_id: i64) -> bool {
        self.get(user_id).map(|s| s.active).unwrap_or(false)
    }

    pub fn active_for(&self, branch: &str) -> Vec<i64> {
        let map = self.subs.lock().unwrap();
        let mut ids: Vec<i64> =
            map.values().filter(|s| s.active && s.branch == branch).map(|s| s.user_id).collect();
        ids.sort_unstable();
        ids
    }

    pub fn active_count(&self) -> usize {
        self.subs.lock().unwrap().values().filter(|s| s.active).count()
    }

    /// Numeric centre id learned from a page (cache survives restarts).
    pub fn center(&self, key: &str) -> Option<u32> {
        self.centers.lock().unwrap().get(key).copied()
    }

    pub fn set_center(&self, key: &str, id: u32) {
        let mut map = self.centers.lock().unwrap();
        if map.get(key) == Some(&id) {
            return;
        }
        map.insert(key.to_string(), id);
        if let Ok(json) = serde_json::to_string_pretty(&*map) {
            if let Err(e) = fs::write(&self.centers_path, json) {
                warn!("cannot save {}: {e}", self.centers_path.display());
            }
        }
    }

    fn save_subs(&self, map: &HashMap<i64, Subscriber>) {
        let as_string_keys: HashMap<String, &Subscriber> =
            map.iter().map(|(k, v)| (k.to_string(), v)).collect();
        match serde_json::to_string_pretty(&as_string_keys) {
            Ok(json) => {
                if let Err(e) = fs::write(&self.subs_path, json) {
                    warn!("cannot save {}: {e}", self.subs_path.display());
                }
            }
            Err(e) => warn!("cannot serialise subscribers: {e}"),
        }
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("cannot parse {}: {e}", path.display());
            None
        }
    }
}
