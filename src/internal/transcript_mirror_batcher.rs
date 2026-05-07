use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep, timeout};

use crate::types::{BoxFutureResult, SessionKey, SessionStore, SessionStoreEntry};

use super::session_store::file_path_to_session_key;

pub const MAX_PENDING_ENTRIES: usize = 500;
pub const MAX_PENDING_BYTES: usize = 1 << 20;
pub const SEND_TIMEOUT_SECONDS: f64 = 60.0;
const MIRROR_APPEND_MAX_ATTEMPTS: usize = 3;
const MIRROR_APPEND_BACKOFF_S: [f64; 2] = [0.2, 0.8];

pub type MirrorErrorCallback =
    Arc<dyn Fn(Option<SessionKey>, String) -> BoxFutureResult<()> + Send + Sync>;

#[derive(Clone)]
struct MirrorEntry {
    file_path: String,
    entries: Vec<SessionStoreEntry>,
}

#[derive(Default)]
struct Pending {
    items: Vec<MirrorEntry>,
    entries: usize,
    bytes: usize,
}

pub struct TranscriptMirrorBatcher {
    store: Arc<dyn SessionStore>,
    projects_dir: String,
    on_error: MirrorErrorCallback,
    send_timeout: Duration,
    max_pending_entries: usize,
    max_pending_bytes: usize,
    pending: Mutex<Pending>,
    append_lock: Mutex<()>,
    inflight_flushes: Mutex<usize>,
    flush_idle: Notify,
}

impl TranscriptMirrorBatcher {
    pub fn new(
        store: Arc<dyn SessionStore>,
        projects_dir: String,
        on_error: MirrorErrorCallback,
        max_pending_entries: usize,
        max_pending_bytes: usize,
    ) -> Self {
        Self {
            store,
            projects_dir,
            on_error,
            send_timeout: Duration::from_secs_f64(SEND_TIMEOUT_SECONDS),
            max_pending_entries,
            max_pending_bytes,
            pending: Mutex::new(Pending::default()),
            append_lock: Mutex::new(()),
            inflight_flushes: Mutex::new(0),
            flush_idle: Notify::new(),
        }
    }

    pub async fn enqueue(self: Arc<Self>, file_path: String, entries: Vec<SessionStoreEntry>) {
        let size = serde_json::to_string(&entries)
            .map(|s| s.len())
            .unwrap_or(0);
        let should_flush = {
            let mut pending = self.pending.lock().await;
            pending.entries += entries.len();
            pending.bytes += size;
            pending.items.push(MirrorEntry { file_path, entries });
            pending.entries > self.max_pending_entries || pending.bytes > self.max_pending_bytes
        };
        if should_flush {
            let this = self.clone();
            *this.inflight_flushes.lock().await += 1;
            tokio::spawn(async move {
                this.drain_pending().await;
                let mut inflight = this.inflight_flushes.lock().await;
                *inflight = inflight.saturating_sub(1);
                if *inflight == 0 {
                    this.flush_idle.notify_waiters();
                    this.flush_idle.notify_one();
                }
            });
        }
    }

    pub async fn flush(&self) {
        self.drain_pending().await;
        loop {
            let notified = {
                let inflight = self.inflight_flushes.lock().await;
                if *inflight == 0 {
                    break;
                }
                self.flush_idle.notified()
            };
            notified.await;
        }
    }

    pub async fn close(&self) {
        self.flush().await;
    }

    async fn drain_pending(&self) {
        let items = {
            let mut pending = self.pending.lock().await;
            let items = std::mem::take(&mut pending.items);
            pending.entries = 0;
            pending.bytes = 0;
            items
        };
        if items.is_empty() {
            return;
        }
        let mut errors = Vec::new();
        {
            let _guard = self.append_lock.lock().await;
            self.do_flush(items, &mut errors).await;
        }
        for (key, error) in errors {
            (self.on_error)(Some(key), error).await;
        }
    }

    async fn do_flush(&self, items: Vec<MirrorEntry>, errors: &mut Vec<(SessionKey, String)>) {
        let mut by_path: HashMap<String, Vec<Value>> = HashMap::new();
        let mut path_order = Vec::new();
        for item in items {
            if !by_path.contains_key(&item.file_path) {
                path_order.push(item.file_path.clone());
            }
            let bucket = by_path.entry(item.file_path).or_default();
            bucket.extend(item.entries);
        }

        for file_path in path_order {
            let entries = by_path.remove(&file_path).unwrap_or_default();
            if entries.is_empty() {
                continue;
            }
            let Some(key) = file_path_to_session_key(&file_path, &self.projects_dir) else {
                tracing::warn!(
                    "[SessionStore] dropping mirror frame: filePath {} is not under {}",
                    file_path,
                    self.projects_dir
                );
                continue;
            };
            let mut last_error = None;
            let mut succeeded = false;
            for attempt in 0..MIRROR_APPEND_MAX_ATTEMPTS {
                if attempt > 0 {
                    sleep(Duration::from_secs_f64(
                        MIRROR_APPEND_BACKOFF_S[attempt - 1],
                    ))
                    .await;
                }
                match timeout(
                    self.send_timeout,
                    self.store.append(key.clone(), entries.clone()),
                )
                .await
                {
                    Ok(Ok(())) => {
                        succeeded = true;
                        break;
                    }
                    Ok(Err(err)) => {
                        last_error = Some(err.to_string());
                    }
                    Err(err) => {
                        last_error = Some(err.to_string());
                        break;
                    }
                }
            }
            if !succeeded {
                let error = last_error.unwrap_or_else(|| "unknown mirror append error".to_string());
                tracing::error!(
                    "[TranscriptMirrorBatcher] flush failed for {}: {}",
                    file_path,
                    error
                );
                errors.push((key, error));
            }
        }
    }
}
