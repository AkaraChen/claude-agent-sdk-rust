use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::errors::Result;
use crate::errors::{ClaudeSdkError, CliConnectionError};
use crate::types::{
    SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry, SessionStoreListEntry,
};

const SUBKEYS: &str = "__subkeys";
const SESSIONS: &str = "__sessions";

#[derive(Debug, Default, Clone)]
pub struct RedisLikeSessionStore {
    prefix: String,
    inner: Arc<Mutex<RedisLikeInner>>,
}

#[derive(Debug, Clone)]
pub struct PostgresLikeSessionStore {
    table: String,
    inner: Arc<Mutex<PostgresLikeInner>>,
}

#[derive(Debug, Clone)]
pub struct S3LikeSessionStore {
    bucket: String,
    prefix: String,
    inner: Arc<Mutex<S3LikeInner>>,
}

#[derive(Debug, Default)]
struct S3LikeInner {
    objects: HashMap<String, Vec<u8>>,
    last_ms: i64,
}

#[derive(Debug, Default)]
struct PostgresLikeInner {
    rows: Vec<PostgresLikeRow>,
    next_seq: i64,
    last_mtime: i64,
}

#[derive(Debug, Clone)]
struct PostgresLikeRow {
    project_key: String,
    session_id: String,
    subpath: String,
    seq: i64,
    entry: SessionStoreEntry,
    mtime: i64,
}

#[derive(Debug, Default)]
struct RedisLikeInner {
    lists: HashMap<String, Vec<String>>,
    sets: HashMap<String, HashSet<String>>,
    zsets: HashMap<String, HashMap<String, i64>>,
    last_mtime: i64,
}

impl RedisLikeSessionStore {
    pub fn new(prefix: impl Into<String>) -> Self {
        let raw = prefix.into();
        let trimmed = raw.trim_end_matches(':');
        let prefix = if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}:")
        };
        Self {
            prefix,
            inner: Arc::new(Mutex::new(RedisLikeInner::default())),
        }
    }

    pub fn entry_key(&self, key: &SessionKey) -> String {
        let mut parts = vec![key.project_key.as_str(), key.session_id.as_str()];
        if let Some(subpath) = key.subpath.as_deref().filter(|s| !s.is_empty()) {
            parts.push(subpath);
        }
        format!("{}{}", self.prefix, parts.join(":"))
    }

    pub fn subkeys_key(&self, key: &SessionListSubkeysKey) -> String {
        format!(
            "{}{}:{}:{}",
            self.prefix, key.project_key, key.session_id, SUBKEYS
        )
    }

    pub fn sessions_key(&self, project_key: &str) -> String {
        format!("{}{project_key}:{SESSIONS}", self.prefix)
    }

    pub async fn raw_list(&self, key: &str) -> Vec<String> {
        self.inner
            .lock()
            .await
            .lists
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn raw_set(&self, key: &str) -> HashSet<String> {
        self.inner
            .lock()
            .await
            .sets
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn raw_zset(&self, key: &str) -> HashMap<String, i64> {
        self.inner
            .lock()
            .await
            .zsets
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn push_raw_list(&self, key: impl Into<String>, values: Vec<String>) {
        self.inner
            .lock()
            .await
            .lists
            .entry(key.into())
            .or_default()
            .extend(values);
    }
}

#[async_trait]
impl SessionStore for RedisLikeSessionStore {
    async fn append(&self, key: SessionKey, entries: Vec<SessionStoreEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut inner = self.inner.lock().await;
        let entry_key = self.entry_key(&key);
        let encoded = entries
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        inner.lists.entry(entry_key).or_default().extend(encoded);

        if let Some(subpath) = key.subpath.as_deref().filter(|s| !s.is_empty()) {
            let subkeys_key = self.subkeys_key(&SessionListSubkeysKey {
                project_key: key.project_key,
                session_id: key.session_id,
            });
            inner
                .sets
                .entry(subkeys_key)
                .or_default()
                .insert(subpath.to_string());
        } else {
            let sessions_key = self.sessions_key(&key.project_key);
            let mtime = inner.next_mtime();
            inner
                .zsets
                .entry(sessions_key)
                .or_default()
                .insert(key.session_id, mtime);
        }
        Ok(())
    }

    async fn load(&self, key: SessionKey) -> Result<Option<Vec<SessionStoreEntry>>> {
        let raw = self
            .inner
            .lock()
            .await
            .lists
            .get(&self.entry_key(&key))
            .cloned()
            .unwrap_or_default();
        if raw.is_empty() {
            return Ok(None);
        }

        let entries = raw
            .into_iter()
            .filter_map(|line| serde_json::from_str::<SessionStoreEntry>(&line).ok())
            .collect::<Vec<_>>();
        Ok((!entries.is_empty()).then_some(entries))
    }

    async fn list_sessions(&self, project_key: String) -> Result<Vec<SessionStoreListEntry>> {
        let mut rows = self
            .inner
            .lock()
            .await
            .zsets
            .get(&self.sessions_key(&project_key))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(session_id, mtime)| SessionStoreListEntry { session_id, mtime })
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| {
            a.mtime
                .cmp(&b.mtime)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(rows)
    }

    async fn delete(&self, key: SessionKey) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if let Some(subpath) = key.subpath.as_deref().filter(|s| !s.is_empty()) {
            let entry_key = self.entry_key(&key);
            inner.lists.remove(&entry_key);
            let subkeys_key = self.subkeys_key(&SessionListSubkeysKey {
                project_key: key.project_key,
                session_id: key.session_id,
            });
            if let Some(set) = inner.sets.get_mut(&subkeys_key) {
                set.remove(subpath);
            }
            return Ok(());
        }

        let list_key = self.entry_key(&key);
        let subkeys_key = self.subkeys_key(&SessionListSubkeysKey {
            project_key: key.project_key.clone(),
            session_id: key.session_id.clone(),
        });
        let subpaths = inner
            .sets
            .remove(&subkeys_key)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        inner.lists.remove(&list_key);
        for subpath in subpaths {
            inner.lists.remove(&self.entry_key(&SessionKey {
                project_key: key.project_key.clone(),
                session_id: key.session_id.clone(),
                subpath: Some(subpath),
            }));
        }
        if let Some(zset) = inner.zsets.get_mut(&self.sessions_key(&key.project_key)) {
            zset.remove(&key.session_id);
        }
        Ok(())
    }

    async fn list_subkeys(&self, key: SessionListSubkeysKey) -> Result<Vec<String>> {
        let mut values = self
            .inner
            .lock()
            .await
            .sets
            .get(&self.subkeys_key(&key))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        values.sort();
        Ok(values)
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_delete(&self) -> bool {
        true
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}

impl RedisLikeInner {
    fn next_mtime(&mut self) -> i64 {
        let mut now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        if now <= self.last_mtime {
            now = self.last_mtime + 1;
        }
        self.last_mtime = now;
        now
    }
}

impl PostgresLikeSessionStore {
    pub fn try_new(table: impl Into<String>) -> Result<Self> {
        let table = table.into();
        if !is_safe_identifier(&table) {
            return Err(ClaudeSdkError::CliConnection(CliConnectionError::new(
                format!(
                    "table {table:?} must match [A-Za-z_][A-Za-z0-9_]* (it is interpolated into SQL)"
                ),
            )));
        }
        Ok(Self {
            table,
            inner: Arc::new(Mutex::new(PostgresLikeInner::default())),
        })
    }

    pub fn new() -> Self {
        Self::try_new("claude_session_store").expect("default table name must be valid")
    }

    pub fn table(&self) -> &str {
        &self.table
    }
}

impl Default for PostgresLikeSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for PostgresLikeSessionStore {
    async fn append(&self, key: SessionKey, entries: Vec<SessionStoreEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let subpath = key.subpath.unwrap_or_default();
        let mut inner = self.inner.lock().await;
        let mtime = inner.next_mtime();
        for entry in entries {
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.rows.push(PostgresLikeRow {
                project_key: key.project_key.clone(),
                session_id: key.session_id.clone(),
                subpath: subpath.clone(),
                seq,
                entry,
                mtime,
            });
        }
        Ok(())
    }

    async fn load(&self, key: SessionKey) -> Result<Option<Vec<SessionStoreEntry>>> {
        let subpath = key.subpath.unwrap_or_default();
        let mut rows = self
            .inner
            .lock()
            .await
            .rows
            .iter()
            .filter(|row| {
                row.project_key == key.project_key
                    && row.session_id == key.session_id
                    && row.subpath == subpath
            })
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| row.seq);
        let entries = rows.into_iter().map(|row| row.entry).collect::<Vec<_>>();
        Ok((!entries.is_empty()).then_some(entries))
    }

    async fn list_sessions(&self, project_key: String) -> Result<Vec<SessionStoreListEntry>> {
        let mut mtimes: HashMap<String, i64> = HashMap::new();
        for row in &self.inner.lock().await.rows {
            if row.project_key == project_key && row.subpath.is_empty() {
                mtimes
                    .entry(row.session_id.clone())
                    .and_modify(|mtime| *mtime = (*mtime).max(row.mtime))
                    .or_insert(row.mtime);
            }
        }
        let mut rows = mtimes
            .into_iter()
            .map(|(session_id, mtime)| SessionStoreListEntry { session_id, mtime })
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Ok(rows)
    }

    async fn delete(&self, key: SessionKey) -> Result<()> {
        let mut inner = self.inner.lock().await;
        match key.subpath {
            Some(subpath) if !subpath.is_empty() => {
                inner.rows.retain(|row| {
                    !(row.project_key == key.project_key
                        && row.session_id == key.session_id
                        && row.subpath == subpath)
                });
            }
            _ => {
                inner.rows.retain(|row| {
                    !(row.project_key == key.project_key && row.session_id == key.session_id)
                });
            }
        }
        Ok(())
    }

    async fn list_subkeys(&self, key: SessionListSubkeysKey) -> Result<Vec<String>> {
        let mut values = self
            .inner
            .lock()
            .await
            .rows
            .iter()
            .filter(|row| {
                row.project_key == key.project_key
                    && row.session_id == key.session_id
                    && !row.subpath.is_empty()
            })
            .map(|row| row.subpath.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        values.sort();
        Ok(values)
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_delete(&self) -> bool {
        true
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}

impl PostgresLikeInner {
    fn next_mtime(&mut self) -> i64 {
        let mut now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        if now <= self.last_mtime {
            now = self.last_mtime + 1;
        }
        self.last_mtime = now;
        now
    }
}

fn is_safe_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

impl S3LikeSessionStore {
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        let raw = prefix.into();
        let trimmed = raw.trim_end_matches('/');
        let prefix = if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}/")
        };
        Self {
            bucket: bucket.into(),
            prefix,
            inner: Arc::new(Mutex::new(S3LikeInner::default())),
        }
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn key_prefix(&self, key: &SessionKey) -> String {
        let mut parts = vec![key.project_key.as_str(), key.session_id.as_str()];
        if let Some(subpath) = key.subpath.as_deref().filter(|s| !s.is_empty()) {
            parts.push(subpath);
        }
        format!("{}{}/", self.prefix, parts.join("/"))
    }

    pub fn project_prefix(&self, project_key: &str) -> String {
        format!("{}{project_key}/", self.prefix)
    }

    pub async fn object_keys(&self) -> Vec<String> {
        let mut keys = self
            .inner
            .lock()
            .await
            .objects
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    pub async fn object_body(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.lock().await.objects.get(key).cloned()
    }

    pub async fn put_raw_object(&self, key: impl Into<String>, body: impl Into<Vec<u8>>) {
        self.inner
            .lock()
            .await
            .objects
            .insert(key.into(), body.into());
    }

    async fn next_part_name(&self) -> String {
        let mut inner = self.inner.lock().await;
        let mut now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        if now <= inner.last_ms {
            now = inner.last_ms + 1;
        }
        inner.last_ms = now;
        let rand = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(6)
            .collect::<String>();
        format!("part-{now:013}-{rand}.jsonl")
    }
}

#[async_trait]
impl SessionStore for S3LikeSessionStore {
    async fn append(&self, key: SessionKey, entries: Vec<SessionStoreEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let object_key = format!("{}{}", self.key_prefix(&key), self.next_part_name().await);
        let mut body = entries
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n");
        body.push('\n');
        self.inner
            .lock()
            .await
            .objects
            .insert(object_key, body.into_bytes());
        Ok(())
    }

    async fn load(&self, key: SessionKey) -> Result<Option<Vec<SessionStoreEntry>>> {
        let prefix = self.key_prefix(&key);
        let mut keys = self
            .inner
            .lock()
            .await
            .objects
            .keys()
            .filter(|object_key| {
                object_key.starts_with(&prefix) && !object_key[prefix.len()..].contains('/')
            })
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        if keys.is_empty() {
            return Ok(None);
        }

        let objects = self.inner.lock().await.objects.clone();
        let mut entries = Vec::new();
        for key in keys {
            let Some(body) = objects.get(&key) else {
                continue;
            };
            let text = String::from_utf8_lossy(body);
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<SessionStoreEntry>(trimmed) {
                    entries.push(value);
                }
            }
        }
        Ok((!entries.is_empty()).then_some(entries))
    }

    async fn list_sessions(&self, project_key: String) -> Result<Vec<SessionStoreListEntry>> {
        let prefix = self.project_prefix(&project_key);
        let mut mtimes: HashMap<String, i64> = HashMap::new();
        for object_key in self.inner.lock().await.objects.keys() {
            let Some(rest) = object_key.strip_prefix(&prefix) else {
                continue;
            };
            let Some(slash) = rest.find('/') else {
                continue;
            };
            if rest[slash + 1..].contains('/') {
                continue;
            }
            let session_id = rest[..slash].to_string();
            let mtime = part_mtime(object_key).unwrap_or_default();
            mtimes
                .entry(session_id)
                .and_modify(|current| *current = (*current).max(mtime))
                .or_insert(mtime);
        }
        let mut rows = mtimes
            .into_iter()
            .map(|(session_id, mtime)| SessionStoreListEntry { session_id, mtime })
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Ok(rows)
    }

    async fn delete(&self, key: SessionKey) -> Result<()> {
        let prefix = self.key_prefix(&key);
        let direct_only = key.subpath.as_deref().is_some_and(|s| !s.is_empty());
        self.inner.lock().await.objects.retain(|object_key, _| {
            if !object_key.starts_with(&prefix) {
                return true;
            }
            if direct_only && object_key[prefix.len()..].contains('/') {
                return true;
            }
            false
        });
        Ok(())
    }

    async fn list_subkeys(&self, key: SessionListSubkeysKey) -> Result<Vec<String>> {
        let prefix = self.key_prefix(&SessionKey {
            project_key: key.project_key,
            session_id: key.session_id,
            subpath: None,
        });
        let mut subkeys = self
            .inner
            .lock()
            .await
            .objects
            .keys()
            .filter_map(|object_key| {
                let rel = object_key.strip_prefix(&prefix)?;
                let parts = rel.split('/').collect::<Vec<_>>();
                (parts.len() >= 2).then(|| parts[..parts.len() - 1].join("/"))
            })
            .filter(|subpath| {
                !subpath.is_empty()
                    && !subpath
                        .split('/')
                        .any(|segment| matches!(segment, "" | "." | ".."))
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        subkeys.sort();
        Ok(subkeys)
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_delete(&self) -> bool {
        true
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}

fn part_mtime(object_key: &str) -> Option<i64> {
    let file = object_key.rsplit('/').next()?;
    let rest = file.strip_prefix("part-")?;
    let (mtime, suffix) = rest.split_once('-')?;
    if mtime.len() != 13 || suffix.len() != "000000.jsonl".len() {
        return None;
    }
    suffix.strip_suffix(".jsonl")?;
    mtime.parse().ok()
}
