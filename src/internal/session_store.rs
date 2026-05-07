use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::types::{
    SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry, SessionStoreListEntry,
    SessionSummaryEntry,
};

use super::session_summary::fold_session_summary;
pub use super::sessions::project_key_for_directory;

#[derive(Debug, Default, Clone)]
pub struct InMemorySessionStore {
    inner: Arc<Mutex<InMemoryInner>>,
}

#[derive(Debug, Default)]
struct InMemoryInner {
    store: HashMap<String, Vec<SessionStoreEntry>>,
    mtimes: HashMap<String, i64>,
    summaries: HashMap<(String, String), SessionSummaryEntry>,
    last_mtime: i64,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_entries(&self, key: SessionKey) -> Vec<SessionStoreEntry> {
        self.inner
            .lock()
            .await
            .store
            .get(&key_to_string(&key))
            .cloned()
            .unwrap_or_default()
    }

    pub async fn size(&self) -> usize {
        self.inner
            .lock()
            .await
            .store
            .keys()
            .filter(|k| k.find('/').is_some_and(|idx| !k[idx + 1..].contains('/')))
            .count()
    }

    pub async fn clear(&self) {
        let mut inner = self.inner.lock().await;
        inner.store.clear();
        inner.mtimes.clear();
        inner.summaries.clear();
        inner.last_mtime = 0;
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn append(&self, key: SessionKey, entries: Vec<SessionStoreEntry>) -> crate::Result<()> {
        let mut inner = self.inner.lock().await;
        let k = key_to_string(&key);
        inner
            .store
            .entry(k.clone())
            .or_default()
            .extend(entries.clone());
        let now_ms = inner.next_mtime();
        if key.subpath.is_none() {
            let sk = (key.project_key.clone(), key.session_id.clone());
            let folded = fold_session_summary(inner.summaries.get(&sk), &key, &entries);
            let mut stamped = folded;
            stamped.mtime = now_ms;
            inner.summaries.insert(sk, stamped);
        }
        inner.mtimes.insert(k, now_ms);
        Ok(())
    }

    async fn load(&self, key: SessionKey) -> crate::Result<Option<Vec<SessionStoreEntry>>> {
        Ok(self
            .inner
            .lock()
            .await
            .store
            .get(&key_to_string(&key))
            .cloned())
    }

    async fn list_sessions(
        &self,
        project_key: String,
    ) -> crate::Result<Vec<SessionStoreListEntry>> {
        let inner = self.inner.lock().await;
        let prefix = format!("{project_key}/");
        let mut results = Vec::new();
        for k in inner.store.keys() {
            if let Some(rest) = k.strip_prefix(&prefix) {
                if !rest.contains('/') {
                    results.push(SessionStoreListEntry {
                        session_id: rest.to_string(),
                        mtime: inner.mtimes.get(k).copied().unwrap_or_default(),
                    });
                }
            }
        }
        Ok(results)
    }

    async fn list_session_summaries(
        &self,
        project_key: String,
    ) -> crate::Result<Vec<SessionSummaryEntry>> {
        Ok(self
            .inner
            .lock()
            .await
            .summaries
            .iter()
            .filter_map(|((pk, _), summary)| (pk == &project_key).then_some(summary.clone()))
            .collect())
    }

    async fn delete(&self, key: SessionKey) -> crate::Result<()> {
        let mut inner = self.inner.lock().await;
        let k = key_to_string(&key);
        inner.store.remove(&k);
        inner.mtimes.remove(&k);
        if key.subpath.is_none() {
            inner
                .summaries
                .remove(&(key.project_key.clone(), key.session_id.clone()));
            let prefix = format!("{}/{}/", key.project_key, key.session_id);
            let keys: Vec<String> = inner
                .store
                .keys()
                .filter(|store_key| store_key.starts_with(&prefix))
                .cloned()
                .collect();
            for store_key in keys {
                inner.store.remove(&store_key);
                inner.mtimes.remove(&store_key);
            }
        }
        Ok(())
    }

    async fn list_subkeys(&self, key: SessionListSubkeysKey) -> crate::Result<Vec<String>> {
        let inner = self.inner.lock().await;
        let prefix = format!("{}/{}/", key.project_key, key.session_id);
        Ok(inner
            .store
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(ToString::to_string))
            .collect())
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_list_session_summaries(&self) -> bool {
        true
    }

    fn supports_delete(&self) -> bool {
        true
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}

impl InMemoryInner {
    fn next_mtime(&mut self) -> i64 {
        let mut now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        if now_ms <= self.last_mtime {
            now_ms = self.last_mtime + 1;
        }
        self.last_mtime = now_ms;
        now_ms
    }
}

pub fn file_path_to_session_key(
    file_path: impl AsRef<Path>,
    projects_dir: impl AsRef<Path>,
) -> Option<SessionKey> {
    let file_path = file_path.as_ref();
    let projects_dir = projects_dir.as_ref();
    let rel = file_path.strip_prefix(projects_dir).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if parts.len() < 2 {
        return None;
    }
    let project_key = parts[0].clone();
    let second = parts[1].clone();
    if parts.len() == 2 && second.ends_with(".jsonl") {
        return Some(SessionKey {
            project_key,
            session_id: second.trim_end_matches(".jsonl").to_string(),
            subpath: None,
        });
    }
    if parts.len() >= 4 {
        let session_id = second;
        let mut subparts = parts[2..].to_vec();
        if let Some(last) = subparts.last_mut() {
            if last.ends_with(".jsonl") {
                *last = last.trim_end_matches(".jsonl").to_string();
            }
        }
        return Some(SessionKey {
            project_key,
            session_id,
            subpath: Some(subparts.join("/")),
        });
    }
    None
}

fn key_to_string(key: &SessionKey) -> String {
    match &key.subpath {
        Some(subpath) if !subpath.is_empty() => {
            format!("{}/{}/{}", key.project_key, key.session_id, subpath)
        }
        _ => format!("{}/{}", key.project_key, key.session_id),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use crate::testing::run_session_store_conformance;

    use super::*;

    #[tokio::test]
    async fn in_memory_store_satisfies_conformance_helper() {
        run_session_store_conformance(
            || Arc::new(InMemorySessionStore::new()) as Arc<dyn SessionStore>,
            HashSet::new(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn file_path_to_session_key_handles_main_and_subagent_paths() {
        let projects = Path::new("/tmp/claude/projects");
        assert_eq!(
            file_path_to_session_key(
                "/tmp/claude/projects/proj/11111111-1111-4111-8111-111111111111.jsonl",
                projects
            ),
            Some(SessionKey {
                project_key: "proj".to_string(),
                session_id: "11111111-1111-4111-8111-111111111111".to_string(),
                subpath: None,
            })
        );
        assert_eq!(
            file_path_to_session_key(
                "/tmp/claude/projects/proj/11111111-1111-4111-8111-111111111111/subagents/agent-abc.jsonl",
                projects
            ),
            Some(SessionKey {
                project_key: "proj".to_string(),
                session_id: "11111111-1111-4111-8111-111111111111".to_string(),
                subpath: Some("subagents/agent-abc".to_string()),
            })
        );
    }
}
