use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;
use redis::{AsyncCommands, Client as RedisClient};
use regex::Regex;
use tokio::sync::Mutex;
use tokio_postgres::NoTls;
use tokio_postgres::types::ToSql;

use crate::errors::{ClaudeSdkError, Result};
use crate::types::{
    SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry, SessionStoreListEntry,
};

const REDIS_SUBKEYS: &str = "__subkeys";
const REDIS_SESSIONS: &str = "__sessions";

#[derive(Debug, Clone)]
pub struct RedisSessionStore {
    client: RedisClient,
    prefix: String,
}

impl RedisSessionStore {
    pub fn new(redis_url: &str, prefix: impl Into<String>) -> Result<Self> {
        let client = RedisClient::open(redis_url).map_err(redis_error)?;
        Ok(Self::from_client(client, prefix))
    }

    pub fn from_client(client: RedisClient, prefix: impl Into<String>) -> Self {
        let raw = prefix.into();
        let trimmed = raw.trim_end_matches(':');
        let prefix = if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}:")
        };
        Self { client, prefix }
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
            self.prefix, key.project_key, key.session_id, REDIS_SUBKEYS
        )
    }

    pub fn sessions_key(&self, project_key: &str) -> String {
        format!("{}{project_key}:{REDIS_SESSIONS}", self.prefix)
    }

    async fn connection(&self) -> Result<redis::aio::MultiplexedConnection> {
        self.client
            .get_multiplexed_async_connection()
            .await
            .map_err(redis_error)
    }
}

#[derive(Debug, Clone)]
pub struct PostgresSessionStore {
    url: String,
    table: String,
}

#[derive(Debug, Clone)]
pub struct S3SessionStore {
    client: S3Client,
    bucket: String,
    prefix: String,
    last_ms: Arc<Mutex<i64>>,
}

impl S3SessionStore {
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>, client: S3Client) -> Self {
        let raw = prefix.into();
        let trimmed = raw.trim_matches('/');
        let prefix = if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}/")
        };
        Self {
            client,
            bucket: bucket.into(),
            prefix,
            last_ms: Arc::new(Mutex::new(0)),
        }
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn key_prefix(&self, key: &SessionKey) -> String {
        let mut path = format!("{}{}/{}", self.prefix, key.project_key, key.session_id);
        if let Some(subpath) = key.subpath.as_deref().filter(|s| !s.is_empty()) {
            path.push('/');
            path.push_str(subpath.trim_matches('/'));
        }
        path.push('/');
        path
    }

    pub fn project_prefix(&self, project_key: &str) -> String {
        format!("{}{project_key}/", self.prefix)
    }

    async fn next_part_key(&self, key: &SessionKey) -> String {
        let mut last_ms = self.last_ms.lock().await;
        let now = current_millis();
        let ms = now.max(*last_ms + 1);
        *last_ms = ms;
        let suffix = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(6)
            .collect::<String>();
        format!("{}part-{ms:013}-{suffix}.jsonl", self.key_prefix(key))
    }

    async fn list_keys(&self, prefix: String) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token = None::<String>;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix);
            if let Some(token_value) = token {
                req = req.continuation_token(token_value);
            }
            let response = req.send().await.map_err(s3_error)?;
            for object in response.contents() {
                if let Some(key) = object.key() {
                    keys.push(key.to_string());
                }
            }
            token = response.next_continuation_token().map(ToString::to_string);
            if token.is_none() {
                break;
            }
        }
        Ok(keys)
    }

    async fn delete_keys(&self, keys: Vec<String>) -> Result<()> {
        for key in keys {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(s3_error)?;
        }
        Ok(())
    }
}

#[async_trait]
impl SessionStore for S3SessionStore {
    async fn append(&self, key: SessionKey, entries: Vec<SessionStoreEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let body = entries
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n")
            + "\n";
        let part_key = self.next_part_key(&key).await;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(part_key)
            .content_type("application/x-ndjson")
            .body(ByteStream::from(body.into_bytes()))
            .send()
            .await
            .map_err(s3_error)?;
        Ok(())
    }

    async fn load(&self, key: SessionKey) -> Result<Option<Vec<SessionStoreEntry>>> {
        let prefix = self.key_prefix(&key);
        let mut part_keys = direct_part_keys(&prefix, self.list_keys(prefix.clone()).await?);
        part_keys.sort();
        if part_keys.is_empty() {
            return Ok(None);
        }

        let mut entries = Vec::new();
        for part_key in part_keys {
            let object = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(part_key)
                .send()
                .await
                .map_err(s3_error)?;
            let bytes = object
                .body
                .collect()
                .await
                .map_err(|error| ClaudeSdkError::Runtime(error.to_string()))?
                .into_bytes();
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                if let Ok(entry) = serde_json::from_str::<SessionStoreEntry>(line) {
                    entries.push(entry);
                }
            }
        }
        if entries.is_empty() {
            Ok(None)
        } else {
            Ok(Some(entries))
        }
    }

    async fn list_sessions(&self, project_key: String) -> Result<Vec<SessionStoreListEntry>> {
        let prefix = self.project_prefix(&project_key);
        let mut by_session = HashMap::<String, i64>::new();
        for key in self.list_keys(prefix.clone()).await? {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let parts = rest.split('/').collect::<Vec<_>>();
            if parts.len() == 2 && is_part_name(parts[1]) {
                let mtime = part_mtime(parts[1]).unwrap_or_default();
                by_session
                    .entry(parts[0].to_string())
                    .and_modify(|existing| *existing = (*existing).max(mtime))
                    .or_insert(mtime);
            }
        }
        let mut sessions = by_session
            .into_iter()
            .map(|(session_id, mtime)| SessionStoreListEntry { session_id, mtime })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Ok(sessions)
    }

    async fn delete(&self, key: SessionKey) -> Result<()> {
        let prefix = self.key_prefix(&key);
        let keys = if key.subpath.as_deref().filter(|s| !s.is_empty()).is_some() {
            direct_part_keys(&prefix, self.list_keys(prefix.clone()).await?)
        } else {
            self.list_keys(prefix).await?
        };
        self.delete_keys(keys).await
    }

    async fn list_subkeys(&self, key: SessionListSubkeysKey) -> Result<Vec<String>> {
        let prefix = format!("{}{}/{}/", self.prefix, key.project_key, key.session_id);
        let mut subkeys = HashSet::new();
        for object_key in self.list_keys(prefix.clone()).await? {
            let Some(rest) = object_key.strip_prefix(&prefix) else {
                continue;
            };
            let mut parts = rest.rsplitn(2, '/');
            let Some(file_name) = parts.next() else {
                continue;
            };
            if !is_part_name(file_name) {
                continue;
            }
            if let Some(subpath) = parts.next().filter(|subpath| !subpath.is_empty()) {
                subkeys.insert(subpath.to_string());
            }
        }
        let mut out = subkeys.into_iter().collect::<Vec<_>>();
        out.sort();
        Ok(out)
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

impl PostgresSessionStore {
    pub fn new(postgres_url: impl Into<String>, table: impl Into<String>) -> Result<Self> {
        let table = table.into();
        validate_table_name(&table)?;
        Ok(Self {
            url: postgres_url.into(),
            table,
        })
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    pub async fn create_schema(&self) -> Result<()> {
        let client = self.connection().await?;
        client
            .execute(
                &format!(
                    "CREATE TABLE IF NOT EXISTS {} (
                        seq BIGSERIAL PRIMARY KEY,
                        project_key TEXT NOT NULL,
                        session_id TEXT NOT NULL,
                        subpath TEXT NOT NULL DEFAULT '',
                        entry JSONB NOT NULL,
                        mtime BIGINT NOT NULL
                    )",
                    self.table
                ),
                &[],
            )
            .await
            .map_err(postgres_error)?;
        client
            .execute(
                &format!(
                    "CREATE INDEX IF NOT EXISTS {0}_lookup_idx
                     ON {0} (project_key, session_id, subpath, seq)",
                    self.table
                ),
                &[],
            )
            .await
            .map_err(postgres_error)?;
        Ok(())
    }

    async fn connection(&self) -> Result<tokio_postgres::Client> {
        let (client, connection) = tokio_postgres::connect(&self.url, NoTls)
            .await
            .map_err(postgres_error)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(client)
    }
}

#[async_trait]
impl SessionStore for PostgresSessionStore {
    async fn append(&self, key: SessionKey, entries: Vec<SessionStoreEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut client = self.connection().await?;
        let subpath = key.subpath.clone().unwrap_or_default();
        let mtime = current_millis();
        let stmt = format!(
            "INSERT INTO {} (project_key, session_id, subpath, entry, mtime)
             VALUES ($1, $2, $3, $4, $5)",
            self.table
        );
        let transaction = client.transaction().await.map_err(postgres_error)?;
        for entry in entries {
            let params: [&(dyn ToSql + Sync); 5] =
                [&key.project_key, &key.session_id, &subpath, &entry, &mtime];
            transaction
                .execute(&stmt, &params)
                .await
                .map_err(postgres_error)?;
        }
        transaction.commit().await.map_err(postgres_error)?;
        Ok(())
    }

    async fn load(&self, key: SessionKey) -> Result<Option<Vec<SessionStoreEntry>>> {
        let client = self.connection().await?;
        let subpath = key.subpath.clone().unwrap_or_default();
        let params: [&(dyn ToSql + Sync); 3] = [&key.project_key, &key.session_id, &subpath];
        let rows = client
            .query(
                &format!(
                    "SELECT entry FROM {}
                     WHERE project_key = $1 AND session_id = $2 AND subpath = $3
                     ORDER BY seq ASC",
                    self.table
                ),
                &params,
            )
            .await
            .map_err(postgres_error)?;
        if rows.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            rows.into_iter()
                .map(|row| row.get::<_, serde_json::Value>(0))
                .collect(),
        ))
    }

    async fn list_sessions(&self, project_key: String) -> Result<Vec<SessionStoreListEntry>> {
        let client = self.connection().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT session_id, MAX(mtime) FROM {}
                     WHERE project_key = $1 AND subpath = ''
                     GROUP BY session_id
                     ORDER BY session_id ASC",
                    self.table
                ),
                &[&project_key],
            )
            .await
            .map_err(postgres_error)?;
        Ok(rows
            .into_iter()
            .map(|row| SessionStoreListEntry {
                session_id: row.get::<_, String>(0),
                mtime: row.get::<_, i64>(1),
            })
            .collect())
    }

    async fn delete(&self, key: SessionKey) -> Result<()> {
        let client = self.connection().await?;
        if let Some(subpath) = key.subpath.as_deref().filter(|s| !s.is_empty()) {
            let params: [&(dyn ToSql + Sync); 3] = [&key.project_key, &key.session_id, &subpath];
            client
                .execute(
                    &format!(
                        "DELETE FROM {}
                         WHERE project_key = $1 AND session_id = $2 AND subpath = $3",
                        self.table
                    ),
                    &params,
                )
                .await
                .map_err(postgres_error)?;
        } else {
            client
                .execute(
                    &format!(
                        "DELETE FROM {}
                         WHERE project_key = $1 AND session_id = $2",
                        self.table
                    ),
                    &[&key.project_key, &key.session_id],
                )
                .await
                .map_err(postgres_error)?;
        }
        Ok(())
    }

    async fn list_subkeys(&self, key: SessionListSubkeysKey) -> Result<Vec<String>> {
        let client = self.connection().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT DISTINCT subpath FROM {}
                     WHERE project_key = $1 AND session_id = $2 AND subpath <> ''
                     ORDER BY subpath ASC",
                    self.table
                ),
                &[&key.project_key, &key.session_id],
            )
            .await
            .map_err(postgres_error)?;
        Ok(rows
            .into_iter()
            .map(|row| row.get::<_, String>(0))
            .collect())
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

fn validate_table_name(table: &str) -> Result<()> {
    let valid = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")
        .expect("valid table-name regex")
        .is_match(table);
    if valid {
        Ok(())
    } else {
        Err(ClaudeSdkError::InvalidOptions(
            "Postgres table name must match [A-Za-z_][A-Za-z0-9_]*".to_string(),
        ))
    }
}

#[async_trait]
impl SessionStore for RedisSessionStore {
    async fn append(&self, key: SessionKey, entries: Vec<SessionStoreEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let encoded = entries
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut conn = self.connection().await?;
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.cmd("RPUSH").arg(self.entry_key(&key)).arg(encoded);
        if let Some(subpath) = key.subpath.as_deref().filter(|s| !s.is_empty()) {
            pipe.cmd("SADD")
                .arg(self.subkeys_key(&SessionListSubkeysKey {
                    project_key: key.project_key.clone(),
                    session_id: key.session_id.clone(),
                }))
                .arg(subpath);
        } else {
            pipe.cmd("ZADD")
                .arg(self.sessions_key(&key.project_key))
                .arg(current_millis())
                .arg(&key.session_id);
        }
        pipe.query_async::<()>(&mut conn).await.map_err(redis_error)
    }

    async fn load(&self, key: SessionKey) -> Result<Option<Vec<SessionStoreEntry>>> {
        let mut conn = self.connection().await?;
        let raw: Vec<String> = conn
            .lrange(self.entry_key(&key), 0, -1)
            .await
            .map_err(redis_error)?;
        if raw.is_empty() {
            return Ok(None);
        }
        let entries = raw
            .into_iter()
            .filter_map(|line| serde_json::from_str::<SessionStoreEntry>(&line).ok())
            .collect::<Vec<_>>();
        if entries.is_empty() {
            Ok(None)
        } else {
            Ok(Some(entries))
        }
    }

    async fn list_sessions(&self, project_key: String) -> Result<Vec<SessionStoreListEntry>> {
        let mut conn = self.connection().await?;
        let raw: Vec<String> = redis::cmd("ZRANGE")
            .arg(self.sessions_key(&project_key))
            .arg(0)
            .arg(-1)
            .arg("WITHSCORES")
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        let mut out = Vec::new();
        for pair in raw.chunks(2) {
            if let [session_id, score] = pair {
                out.push(SessionStoreListEntry {
                    session_id: session_id.clone(),
                    mtime: parse_redis_score(score),
                });
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: SessionKey) -> Result<()> {
        let mut conn = self.connection().await?;
        if let Some(subpath) = key.subpath.as_deref().filter(|s| !s.is_empty()) {
            let mut pipe = redis::pipe();
            pipe.atomic();
            pipe.cmd("DEL").arg(self.entry_key(&key));
            pipe.cmd("SREM")
                .arg(self.subkeys_key(&SessionListSubkeysKey {
                    project_key: key.project_key.clone(),
                    session_id: key.session_id.clone(),
                }))
                .arg(subpath);
            return pipe.query_async::<()>(&mut conn).await.map_err(redis_error);
        }

        let subkeys_key = self.subkeys_key(&SessionListSubkeysKey {
            project_key: key.project_key.clone(),
            session_id: key.session_id.clone(),
        });
        let subpaths: Vec<String> = conn.smembers(&subkeys_key).await.map_err(redis_error)?;
        let mut to_delete = vec![self.entry_key(&key), subkeys_key];
        for subpath in subpaths {
            to_delete.push(self.entry_key(&SessionKey {
                project_key: key.project_key.clone(),
                session_id: key.session_id.clone(),
                subpath: Some(subpath),
            }));
        }

        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.cmd("DEL").arg(to_delete);
        pipe.cmd("ZREM")
            .arg(self.sessions_key(&key.project_key))
            .arg(&key.session_id);
        pipe.query_async::<()>(&mut conn).await.map_err(redis_error)
    }

    async fn list_subkeys(&self, key: SessionListSubkeysKey) -> Result<Vec<String>> {
        let mut conn = self.connection().await?;
        let mut subkeys: Vec<String> = conn
            .smembers(self.subkeys_key(&key))
            .await
            .map_err(redis_error)?;
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

fn current_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn parse_redis_score(score: &str) -> i64 {
    score
        .parse::<i64>()
        .or_else(|_| score.parse::<f64>().map(|v| v as i64))
        .unwrap_or_default()
}

fn direct_part_keys(prefix: &str, keys: Vec<String>) -> Vec<String> {
    keys.into_iter()
        .filter(|key| {
            key.strip_prefix(prefix)
                .is_some_and(|rest| is_part_name(rest) && !rest.contains('/'))
        })
        .collect()
}

fn is_part_name(name: &str) -> bool {
    name.starts_with("part-") && name.ends_with(".jsonl")
}

fn part_mtime(name: &str) -> Option<i64> {
    name.strip_prefix("part-")?.get(..13)?.parse().ok()
}

fn redis_error(error: redis::RedisError) -> ClaudeSdkError {
    ClaudeSdkError::Runtime(error.to_string())
}

fn postgres_error(error: tokio_postgres::Error) -> ClaudeSdkError {
    ClaudeSdkError::Runtime(error.to_string())
}

fn s3_error<E: std::fmt::Display>(error: E) -> ClaudeSdkError {
    ClaudeSdkError::Runtime(error.to_string())
}
