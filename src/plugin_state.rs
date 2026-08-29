//! Persistent plugin-owned state, separate from Agent sessions.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio_rusqlite::{
    Connection,
    rusqlite::{OptionalExtension, params},
};

pub const PLUGIN_STATE_DATABASE: &str = "plugin-state-v1.db";
const MAX_NAME_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_LIST_LIMIT: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginStateScope {
    Global,
    Project,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PluginStateEntry {
    pub key: String,
    pub value: Value,
    pub revision: u64,
}

#[derive(Clone)]
pub struct PluginStateStore {
    connection: Connection,
    project: Vec<u8>,
}

#[derive(Clone)]
pub struct PluginState {
    store: PluginStateStore,
    namespace: String,
    scope: PluginStateScope,
}

impl PluginStateStore {
    /// Opens an explicit database path and binds project-scoped handles to `project_cwd`.
    pub async fn open(path: impl AsRef<Path>, project_cwd: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!("cannot create plugin state directory {}", parent.display())
        })?;
        secure_directory(parent).await?;

        let project_cwd = tokio::fs::canonicalize(project_cwd.as_ref())
            .await
            .context("cannot canonicalize plugin state project cwd")?;
        let connection = Connection::open(path)
            .await
            .with_context(|| format!("cannot open plugin state database {}", path.display()))?;
        secure_file(path).await?;
        connection
            .call(|connection| {
                connection.execute_batch(
                    "PRAGMA journal_mode = WAL;
                     PRAGMA foreign_keys = ON;
                     CREATE TABLE IF NOT EXISTS plugin_state (
                         namespace TEXT NOT NULL,
                         scope INTEGER NOT NULL,
                         project BLOB NOT NULL,
                         key TEXT NOT NULL,
                         value TEXT NOT NULL,
                         revision INTEGER NOT NULL CHECK (revision > 0),
                         PRIMARY KEY (namespace, scope, project, key)
                     );",
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .context("cannot initialize plugin state database")?;
        Ok(Self {
            connection,
            project: path_identity(&project_cwd),
        })
    }

    pub fn state(
        &self,
        namespace: impl Into<String>,
        scope: PluginStateScope,
    ) -> Result<PluginState> {
        let namespace = namespace.into();
        validate_name("namespace", &namespace)?;
        Ok(PluginState {
            store: self.clone(),
            namespace,
            scope,
        })
    }
}

impl PluginState {
    pub(crate) fn scoped(
        &self,
        namespace: impl Into<String>,
        scope: PluginStateScope,
    ) -> Result<Self> {
        self.store.state(namespace, scope)
    }

    pub(crate) fn with_scope(&self, scope: PluginStateScope) -> Result<Self> {
        self.store.state(self.namespace.clone(), scope)
    }

    pub async fn get(&self, key: &str) -> Result<Option<PluginStateEntry>> {
        validate_name("key", key)?;
        let (namespace, scope, project, key) = self.query_parts(key);
        self.store
            .connection
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT value, revision FROM plugin_state
                         WHERE namespace = ?1 AND scope = ?2 AND project = ?3 AND key = ?4",
                        params![namespace, scope, project, key],
                        |row| decode_entry(key.clone(), row.get(0)?, row.get(1)?),
                    )
                    .optional()
            })
            .await
            .context("cannot read plugin state")
    }

    pub async fn put(&self, key: &str, value: Value) -> Result<PluginStateEntry> {
        validate_name("key", key)?;
        let value = encode_value(&value)?;
        let (namespace, scope, project, key) = self.query_parts(key);
        let result_key = key.clone();
        let (value, revision) = self
            .store
            .connection
            .call(move |connection| {
                connection.query_row(
                    "INSERT INTO plugin_state (namespace, scope, project, key, value, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT(namespace, scope, project, key) DO UPDATE
                 SET value = excluded.value, revision = plugin_state.revision + 1
                 RETURNING value, revision",
                    params![namespace, scope, project, key, value],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
            })
            .await
            .context("cannot write plugin state")?;
        decode_entry(result_key, value, revision).context("cannot decode written plugin state")
    }

    pub async fn delete(&self, key: &str) -> Result<bool> {
        validate_name("key", key)?;
        let (namespace, scope, project, key) = self.query_parts(key);
        Ok(self.store.connection.call(move |connection| {
            connection.execute(
                "DELETE FROM plugin_state WHERE namespace = ?1 AND scope = ?2 AND project = ?3 AND key = ?4",
                params![namespace, scope, project, key],
            )
        }).await.context("cannot delete plugin state")? != 0)
    }

    pub async fn list(&self, prefix: &str, limit: usize) -> Result<Vec<PluginStateEntry>> {
        if !prefix.is_empty() {
            validate_name("prefix", prefix)?;
        }
        if limit == 0 || limit > MAX_LIST_LIMIT {
            bail!("list limit must be between 1 and {MAX_LIST_LIMIT}");
        }
        let (namespace, scope, project, _) = self.query_parts("");
        let prefix = prefix.to_owned();
        self.store
            .connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT key, value, revision FROM plugin_state
                 WHERE namespace = ?1 AND scope = ?2 AND project = ?3
                   AND substr(key, 1, length(?4)) = ?4
                 ORDER BY key ASC LIMIT ?5",
                )?;
                let rows = statement.query_map(
                    params![namespace, scope, project, prefix, limit as i64],
                    |row| decode_entry(row.get(0)?, row.get(1)?, row.get(2)?),
                )?;
                rows.collect()
            })
            .await
            .context("cannot list plugin state")
    }

    /// Atomically writes when `expected_revision` matches. `None` means the key must not exist.
    /// A conflict returns `Ok(None)`.
    pub async fn compare_and_set(
        &self,
        key: &str,
        expected_revision: Option<u64>,
        value: Value,
    ) -> Result<Option<PluginStateEntry>> {
        validate_name("key", key)?;
        if expected_revision == Some(0) {
            bail!("expected revision must be greater than zero");
        }
        let value = encode_value(&value)?;
        let (namespace, scope, project, key) = self.query_parts(key);
        let result_key = key.clone();
        let row =
            self.store
                .connection
                .call(move |connection| {
                    match expected_revision {
            None => connection.query_row(
                "INSERT INTO plugin_state (namespace, scope, project, key, value, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT(namespace, scope, project, key) DO NOTHING
                 RETURNING value, revision",
                params![namespace, scope, project, key, value],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            ).optional(),
            Some(expected) => connection.query_row(
                "UPDATE plugin_state SET value = ?5, revision = revision + 1
                 WHERE namespace = ?1 AND scope = ?2 AND project = ?3 AND key = ?4 AND revision = ?6
                 RETURNING value, revision",
                params![namespace, scope, project, key, value, expected as i64],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            ).optional(),
        }
                })
                .await
                .context("cannot compare and set plugin state")?;
        row.map(|(value, revision)| decode_entry(result_key, value, revision))
            .transpose()
            .context("cannot decode written plugin state")
    }

    fn query_parts(&self, key: &str) -> (String, i64, Vec<u8>, String) {
        let (scope, project) = match self.scope {
            PluginStateScope::Global => (0, Vec::new()),
            PluginStateScope::Project => (1, self.store.project.clone()),
        };
        (self.namespace.clone(), scope, project, key.to_owned())
    }
}

fn validate_name(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_NAME_BYTES {
        bail!("{label} must be between 1 and {MAX_NAME_BYTES} bytes");
    }
    if value
        .chars()
        .any(|character| character.is_control() || matches!(character, '.' | '/' | '\\'))
    {
        bail!("{label} contains a forbidden character");
    }
    Ok(())
}

fn encode_value(value: &Value) -> Result<String> {
    let encoded = serde_json::to_string(value).context("cannot encode plugin state value")?;
    if encoded.len() > MAX_VALUE_BYTES {
        bail!("plugin state value exceeds {MAX_VALUE_BYTES} bytes");
    }
    Ok(encoded)
}

fn decode_entry(
    key: String,
    value: String,
    revision: i64,
) -> tokio_rusqlite::rusqlite::Result<PluginStateEntry> {
    let value = serde_json::from_str(&value).map_err(|error| {
        tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
            0,
            tokio_rusqlite::rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    Ok(PluginStateEntry {
        key,
        value,
        revision: revision as u64,
    })
}

#[cfg(unix)]
fn path_identity(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_identity(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
async fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .with_context(|| format!("cannot secure plugin state directory {}", path.display()))
}

#[cfg(not(unix))]
async fn secure_directory(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
async fn secure_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .with_context(|| format!("cannot secure plugin state database {}", path.display()))
}

#[cfg(not(unix))]
async fn secure_file(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn store(root: &Path, project: &Path) -> PluginStateStore {
        PluginStateStore::open(root.join(PLUGIN_STATE_DATABASE), project)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn isolates_namespace_scope_and_project() {
        let temp = tempfile::tempdir().unwrap();
        let p1 = temp.path().join("p1");
        let p2 = temp.path().join("p2");
        tokio::fs::create_dir_all(&p1).await.unwrap();
        tokio::fs::create_dir_all(&p2).await.unwrap();
        let first = store(temp.path(), &p1).await;
        let second = store(temp.path(), &p2).await;
        first
            .state("one", PluginStateScope::Project)
            .unwrap()
            .put("key", json!(1))
            .await
            .unwrap();
        first
            .state("one", PluginStateScope::Global)
            .unwrap()
            .put("key", json!(2))
            .await
            .unwrap();
        first
            .state("two", PluginStateScope::Project)
            .unwrap()
            .put("key", json!(3))
            .await
            .unwrap();
        assert!(
            second
                .state("one", PluginStateScope::Project)
                .unwrap()
                .get("key")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            second
                .state("one", PluginStateScope::Global)
                .unwrap()
                .get("key")
                .await
                .unwrap()
                .unwrap()
                .value,
            json!(2)
        );
    }

    #[tokio::test]
    async fn persists_across_reopen_and_increments_revision() {
        let temp = tempfile::tempdir().unwrap();
        let entry = store(temp.path(), temp.path())
            .await
            .state("plugin", PluginStateScope::Project)
            .unwrap()
            .put("key", json!({"saved": true}))
            .await
            .unwrap();
        assert_eq!(entry.revision, 1);
        let reopened = store(temp.path(), temp.path())
            .await
            .state("plugin", PluginStateScope::Project)
            .unwrap();
        assert_eq!(reopened.get("key").await.unwrap().unwrap(), entry);
        assert_eq!(reopened.put("key", json!(2)).await.unwrap().revision, 2);
    }

    #[tokio::test]
    async fn compare_and_set_reports_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let state = store(temp.path(), temp.path())
            .await
            .state("plugin", PluginStateScope::Global)
            .unwrap();
        assert_eq!(
            state
                .compare_and_set("key", None, json!(1))
                .await
                .unwrap()
                .unwrap()
                .revision,
            1
        );
        assert!(
            state
                .compare_and_set("key", None, json!(2))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .compare_and_set("key", Some(9), json!(2))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state
                .compare_and_set("key", Some(1), json!(2))
                .await
                .unwrap()
                .unwrap()
                .revision,
            2
        );
    }

    #[tokio::test]
    async fn lists_prefix_in_key_order() {
        let temp = tempfile::tempdir().unwrap();
        let state = store(temp.path(), temp.path())
            .await
            .state("plugin", PluginStateScope::Global)
            .unwrap();
        for key in ["beta", "alphaTwo", "alphaOne"] {
            state.put(key, json!(key)).await.unwrap();
        }
        let keys: Vec<_> = state
            .list("alpha", 10)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.key)
            .collect();
        assert_eq!(keys, ["alphaOne", "alphaTwo"]);
    }

    #[tokio::test]
    async fn rejects_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path(), temp.path()).await;
        assert!(store.state("", PluginStateScope::Global).is_err());
        assert!(store.state("bad.name", PluginStateScope::Global).is_err());
        let state = store.state("plugin", PluginStateScope::Global).unwrap();
        for key in ["", "bad/key", "bad\\key", "bad\nkey"] {
            assert!(state.get(key).await.is_err());
        }
        assert!(state.get(&"x".repeat(129)).await.is_err());
        assert!(
            state
                .put("key", json!("x".repeat(MAX_VALUE_BYTES)))
                .await
                .is_err()
        );
        assert!(state.list("", 0).await.is_err());
        assert!(state.list("", MAX_LIST_LIMIT + 1).await.is_err());
        assert!(
            state
                .compare_and_set("key", Some(0), json!(1))
                .await
                .is_err()
        );
    }
}
