//! Shared application state: config, store, AI settings, and per-project
//! runtime handles (event broadcast, cancellation, live progress).

use crate::config::Config;
use crate::settings::AiSettings;
use crate::store::Store;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize, Debug)]
pub struct LiveStage {
    pub stage: String,
    pub progress: f32,
    pub detail: Option<String>,
}

pub struct ProjectHandle {
    pub events: broadcast::Sender<String>,
    pub cancel: Mutex<CancellationToken>,
    pub running: AtomicBool,
    /// In-memory progress for the currently executing stage. Durable state
    /// transitions live in project.json; this fills the gaps between them.
    pub live: Mutex<Option<LiveStage>>,
}

impl ProjectHandle {
    fn new() -> Arc<ProjectHandle> {
        let (tx, _) = broadcast::channel(512);
        Arc::new(ProjectHandle {
            events: tx,
            cancel: Mutex::new(CancellationToken::new()),
            running: AtomicBool::new(false),
            live: Mutex::new(None),
        })
    }

    pub fn emit(&self, value: serde_json::Value) {
        let _ = self.events.send(value.to_string());
    }

    pub fn set_live(&self, stage: &str, progress: f32, detail: Option<String>) {
        *self.live.lock().unwrap() = Some(LiveStage {
            stage: stage.to_string(),
            progress,
            detail,
        });
    }

    pub fn clear_live(&self) {
        *self.live.lock().unwrap() = None;
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Store,
    pub settings: Arc<RwLock<AiSettings>>,
    handles: Arc<Mutex<HashMap<String, Arc<ProjectHandle>>>>,
    /// `project-id/clip-id` keys with a caption restyle currently running.
    restyling: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    pub fn new(cfg: Config) -> AppState {
        let store = Store::new(&cfg.data_dir);
        let settings = crate::settings::load(&cfg.data_dir);
        AppState {
            cfg: Arc::new(cfg),
            store,
            settings: Arc::new(RwLock::new(settings)),
            handles: Arc::new(Mutex::new(HashMap::new())),
            restyling: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn handle(&self, id: &str) -> Arc<ProjectHandle> {
        let mut map = self.handles.lock().unwrap();
        map.entry(id.to_string())
            .or_insert_with(ProjectHandle::new)
            .clone()
    }

    /// Claim the restyle lock for one clip. Returns false when a restyle for
    /// the same clip is already running.
    pub fn try_begin_restyle(&self, key: &str) -> bool {
        self.restyling.lock().unwrap().insert(key.to_string())
    }

    pub fn end_restyle(&self, key: &str) {
        self.restyling.lock().unwrap().remove(key);
    }
}
