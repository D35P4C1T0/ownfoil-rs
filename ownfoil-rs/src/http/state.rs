use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::auth::AuthSettings;
use crate::catalog::Catalog;
use crate::settings::Settings;
use crate::shop::ShopConfig;
use crate::storage::Storage;
use crate::titledb::TitleDb;

/// Session token -> (username, `expires_at`). Sessions expire after 24 hours.
#[derive(Debug, Clone)]
pub struct SessionStore {
    inner: Arc<DashMap<String, (String, Instant)>>,
    ttl: Duration,
}

impl SessionStore {
    pub fn new(ttl_hours: u64) -> Self {
        Self { inner: Arc::new(DashMap::new()), ttl: Duration::from_secs(ttl_hours * 3600) }
    }

    pub fn create(&self, username: String) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let expires = Instant::now() + self.ttl;
        self.inner.insert(token.clone(), (username, expires));
        token
    }

    pub fn get(&self, token: &str) -> Option<String> {
        let entry = self.inner.get(token)?;
        if entry.1 > Instant::now() {
            Some(entry.0.clone())
        } else {
            drop(entry);
            self.inner.remove(token);
            None
        }
    }

    pub fn remove(&self, token: &str) {
        self.inner.remove(token);
    }
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub catalog: Arc<RwLock<Catalog>>,
    pub library_root: PathBuf,
    pub storage: Option<Storage>,
    pub scan_lock: Arc<Mutex<()>>,
    pub settings: Arc<RwLock<Settings>>,
    pub settings_path: PathBuf,
    pub keys_path: PathBuf,
    pub auth: Arc<AuthSettings>,
    pub shop: Arc<RwLock<ShopConfig>>,
    pub insecure_admin_cookie: bool,
    pub sessions: SessionStore,
    pub titledb: TitleDb,
    pub titles_cache: Arc<RwLock<Option<(u64, serde_json::Value)>>>,
    pub data_dir: PathBuf,
    pub titledb_progress_tx: broadcast::Sender<String>,
}
