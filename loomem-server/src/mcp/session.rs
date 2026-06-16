use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const MAX_SESSIONS: usize = 200;
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

pub struct SessionState {
    pub initialized: bool,
    pub created_at: Instant,
    pub last_accessed: Instant,
}

pub type SessionStore = Arc<RwLock<HashMap<String, SessionState>>>;

pub fn new_session_store() -> SessionStore {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Insert a new session, evicting expired and then oldest if still at capacity.
pub async fn create_session(store: &SessionStore) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let mut map = store.write().await;

    // Evict expired sessions first
    let now = Instant::now();
    map.retain(|_, s| now.duration_since(s.last_accessed) < SESSION_TTL);

    // If still at capacity, evict least recently used
    if map.len() >= MAX_SESSIONS {
        if let Some(oldest_id) = map
            .iter()
            .min_by_key(|(_, s)| s.last_accessed)
            .map(|(id, _)| id.clone())
        {
            map.remove(&oldest_id);
        }
    }

    map.insert(
        id.clone(),
        SessionState {
            initialized: true,
            created_at: Instant::now(),
            last_accessed: Instant::now(),
        },
    );
    id
}

/// Check if session exists and update last_accessed timestamp.
pub async fn get_session(store: &SessionStore, id: &str) -> bool {
    let mut map = store.write().await;
    if let Some(state) = map.get_mut(id) {
        let now = Instant::now();
        if now.duration_since(state.last_accessed) >= SESSION_TTL {
            map.remove(id);
            return false;
        }
        state.last_accessed = now;
        true
    } else {
        false
    }
}

pub async fn remove_session(store: &SessionStore, id: &str) {
    store.write().await.remove(id);
}
