use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use indexfox_core::AppConfig;

use crate::scheduler::{Database, IndexScheduler};
use crate::storage::DatabaseLocation;

/// Process-wide state shared by the Tauri commands.
///
/// Database connections are opened per blocking task instead of being cached
/// here: `Connection` is `Send` but not `Sync`, and independent WAL readers
/// keep a long index run from blocking searches.
pub struct AppState {
    pub config: AppConfig,
    pub database: Database,
    pub scheduler: IndexScheduler,
    cancels: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let database = Arc::new(RwLock::new(DatabaseLocation::load(
            config.db_path.clone(),
            config.data_dir.join("config").join("database.json"),
        )));
        Self {
            config,
            scheduler: IndexScheduler::new(Arc::clone(&database)),
            database,
            cancels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register one in-flight search id.
    ///
    /// A duplicate id is rejected rather than replacing the existing token;
    /// otherwise a late cancellation could be delivered to the wrong search or
    /// two responses could race under one id.
    pub fn begin_search(&self, request_id: u64) -> Result<SearchGuard, String> {
        let mut cancels = self.cancels.lock().unwrap_or_else(PoisonError::into_inner);
        if cancels.contains_key(&request_id) {
            return Err(format!("search request id {request_id} is already running"));
        }
        let token = Arc::new(AtomicBool::new(false));
        cancels.insert(request_id, Arc::clone(&token));
        Ok(SearchGuard {
            request_id,
            token,
            cancels: Arc::clone(&self.cancels),
        })
    }

    /// Ask the search registered under `request_id` to stop. Unknown ids are
    /// treated as already cancelled; the guard removes the entry on completion.
    pub fn cancel(&self, request_id: u64) {
        let cancels = self.cancels.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(token) = cancels.get(&request_id) {
            token.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Removes a completed search's cancellation entry, but only if the entry is
/// still the token this search registered.
pub struct SearchGuard {
    request_id: u64,
    token: Arc<AtomicBool>,
    cancels: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
}

impl SearchGuard {
    pub fn token(&self) -> &Arc<AtomicBool> {
        &self.token
    }
}

impl Drop for SearchGuard {
    fn drop(&mut self) {
        let mut cancels = self.cancels.lock().unwrap_or_else(PoisonError::into_inner);
        let owned = cancels
            .get(&self.request_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.token));
        if owned {
            cancels.remove(&self.request_id);
        }
    }
}
