use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::auth::AuthSettings;
use crate::catalog::Catalog;

#[derive(Debug, Clone)]
pub struct AppState {
    pub catalog: Arc<RwLock<Catalog>>,
    pub library_root: PathBuf,
    pub auth: Arc<AuthSettings>,
}
