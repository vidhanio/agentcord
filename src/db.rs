use std::{collections::HashSet, path::Path};

use rusqlite::{Connection, OptionalExtension, params};
use serenity::all::{GenericChannelId, MessageId};

use crate::{BotError, BotResult};

/// Minimal persisted tuple needed to restore a session-thread binding.
#[derive(Clone, Debug)]
pub struct SessionRow {
    /// Discord forum thread that represents the session.
    pub thread_id: GenericChannelId,
    /// Agent-owned ACP session identifier.
    pub session_id: String,
    /// Configured agent key used to spawn the correct executable.
    pub agent_key: String,
    /// Working directory used to load the session.
    pub project_path: String,
}

/// Persisted mapping from one ACP source to its Discord projection.
#[derive(Clone, Debug)]
pub struct RenderRow {
    /// Stable logical key for a message, turn, tool call, or metadata item.
    pub source_key: String,
    /// Discord messages currently representing this source.
    pub discord_message_ids: Vec<MessageId>,
    /// Renderer-specific accumulated source state.
    pub state_json: String,
}

/// Serialized access to Agentcord's SQLite state database.
#[derive(Debug)]
pub struct Db {
    /// Single rusqlite connection shared by application tasks.
    connection: std::sync::Mutex<Connection>,
}

impl Db {
    /// Opens the state database and creates its parent directory if needed.
    pub fn open(path: &Path) -> BotResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// Initializes schema and migrations for an existing SQLite connection.
    fn from_connection(connection: Connection) -> BotResult<Self> {
        // Legacy rows carry only derivable extras (availability, titles,
        // capability caches); they are rewritten into the minimal shape.
        connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
        Self::migrate_legacy(&connection)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
               thread_id TEXT PRIMARY KEY,
               session_id TEXT NOT NULL,
               agent_key TEXT NOT NULL,
               project_path TEXT NOT NULL,
               UNIQUE(agent_key, session_id)
             );
             CREATE TABLE IF NOT EXISTS renders (
               thread_id TEXT NOT NULL REFERENCES sessions(thread_id) ON DELETE CASCADE,
               source_key TEXT NOT NULL,
               discord_message_ids TEXT NOT NULL,
               state_json TEXT NOT NULL,
               PRIMARY KEY(thread_id, source_key)
             );
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(Self {
            connection: std::sync::Mutex::new(connection),
        })
    }

    /// Rewrites databases from the schema that cached derivable session
    /// fields (`availability`, `title`, `turn`, capability snapshots, ...)
    /// into the minimal restore-tuple schema, preserving render state.
    fn migrate_legacy(connection: &Connection) -> BotResult {
        let legacy: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'availability'",
            [],
            |row| row.get(0),
        )?;
        if legacy == 0 {
            return Ok(());
        }
        connection.execute_batch(
            "ALTER TABLE renders RENAME TO renders_legacy;
             ALTER TABLE sessions RENAME TO sessions_legacy;
             CREATE TABLE sessions (
               thread_id TEXT PRIMARY KEY,
               session_id TEXT NOT NULL,
               agent_key TEXT NOT NULL,
               project_path TEXT NOT NULL,
               UNIQUE(agent_key, session_id)
             );
             CREATE TABLE renders (
               thread_id TEXT NOT NULL REFERENCES sessions(thread_id) ON DELETE CASCADE,
               source_key TEXT NOT NULL,
               discord_message_ids TEXT NOT NULL,
               state_json TEXT NOT NULL,
               PRIMARY KEY(thread_id, source_key)
             );
             INSERT INTO sessions (thread_id, session_id, agent_key, project_path)
               SELECT thread_id, session_id, agent_key, project_path FROM sessions_legacy;
             INSERT INTO renders (thread_id, source_key, discord_message_ids, state_json)
               SELECT thread_id, source_key, discord_message_ids, state_json FROM renders_legacy;
             DROP TABLE renders_legacy;
             DROP TABLE sessions_legacy;",
        )?;
        Ok(())
    }

    /// Persists the irreducible tuple needed to restore an ACP session.
    pub fn insert_session(&self, row: &SessionRow) -> BotResult {
        self.connection()?.execute(
            "INSERT INTO sessions (thread_id, session_id, agent_key, project_path)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                row.thread_id.to_string(),
                row.session_id,
                row.agent_key,
                row.project_path,
            ],
        )?;
        Ok(())
    }

    /// Looks up the session bound to a Discord thread.
    pub fn session(&self, thread_id: GenericChannelId) -> BotResult<Option<SessionRow>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT thread_id, session_id, agent_key, project_path
                 FROM sessions WHERE thread_id = ?1",
                [thread_id.to_string()],
                map_session,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Lists every persisted session in insertion order.
    pub fn sessions(&self) -> BotResult<Vec<SessionRow>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT thread_id, session_id, agent_key, project_path
             FROM sessions ORDER BY rowid",
        )?;
        let rows = statement.query_map([], map_session)?;
        let sessions = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        drop(connection);
        Ok(sessions)
    }

    /// Looks up a session by agent key and ACP session id.
    pub fn agent_session(
        &self,
        agent_key: &str,
        session_id: &str,
    ) -> BotResult<Option<SessionRow>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT thread_id, session_id, agent_key, project_path
                 FROM sessions WHERE agent_key = ?1 AND session_id = ?2",
                params![agent_key, session_id],
                map_session,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Returns all agent/session pairs already imported into Discord.
    pub fn session_keys(&self) -> BotResult<HashSet<(String, String)>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT agent_key, session_id FROM sessions")?;
        let keys = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<HashSet<_>, _>>()?;
        drop(statement);
        drop(connection);
        Ok(keys)
    }

    /// Deletes a session and its cascading render state.
    pub fn delete_session(&self, thread_id: GenericChannelId) -> BotResult {
        self.connection()?.execute(
            "DELETE FROM sessions WHERE thread_id = ?1",
            [thread_id.to_string()],
        )?;
        Ok(())
    }

    /// The highest turn number that already rendered for a thread.
    pub fn latest_turn(&self, thread_id: GenericChannelId) -> BotResult<u64> {
        let connection = self.connection()?;
        let latest: i64 = connection.query_row(
            "SELECT COALESCE(MAX(CAST(substr(source_key, 6) AS INTEGER)), 0)
             FROM renders WHERE thread_id = ?1 AND source_key LIKE 'turn:%'",
            [thread_id.to_string()],
            |row| row.get(0),
        )?;
        drop(connection);
        Ok(u64::try_from(latest).unwrap_or_default())
    }

    /// Reserves the next turn's render row so concurrent turns cannot reuse
    /// its key, and returns the turn number.
    pub fn begin_turn(&self, thread_id: GenericChannelId) -> BotResult<u64> {
        let connection = self.connection()?;
        let latest: i64 = connection.query_row(
            "SELECT COALESCE(MAX(CAST(substr(source_key, 6) AS INTEGER)), 0)
             FROM renders WHERE thread_id = ?1 AND source_key LIKE 'turn:%'",
            [thread_id.to_string()],
            |row| row.get(0),
        )?;
        let turn = u64::try_from(latest).unwrap_or_default() + 1;
        connection.execute(
            "INSERT OR IGNORE INTO renders (thread_id, source_key, discord_message_ids, state_json)
             VALUES (?1, ?2, '[]', '{}')",
            params![thread_id.to_string(), format!("turn:{turn}:response")],
        )?;
        drop(connection);
        Ok(turn)
    }

    /// Loads the persisted Discord projection for one logical source.
    pub fn render(
        &self,
        thread_id: GenericChannelId,
        source_key: &str,
    ) -> BotResult<Option<RenderRow>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT source_key, discord_message_ids, state_json
                 FROM renders WHERE thread_id = ?1 AND source_key = ?2",
                params![thread_id.to_string(), source_key],
                |row| {
                    let ids: String = row.get(1)?;
                    let ids = serde_json::from_str::<Vec<u64>>(&ids)
                        .unwrap_or_default()
                        .into_iter()
                        .map(MessageId::new)
                        .collect();
                    Ok(RenderRow {
                        source_key: row.get(0)?,
                        discord_message_ids: ids,
                        state_json: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Inserts or replaces the Discord projection for one logical source.
    pub fn upsert_render(&self, thread_id: GenericChannelId, row: &RenderRow) -> BotResult {
        let ids = row
            .discord_message_ids
            .iter()
            .map(|id| id.get())
            .collect::<Vec<_>>();
        self.connection()?.execute(
            "INSERT INTO renders (thread_id, source_key, discord_message_ids, state_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(thread_id, source_key) DO UPDATE SET
               discord_message_ids = excluded.discord_message_ids,
               state_json = excluded.state_json",
            params![
                thread_id.to_string(),
                row.source_key,
                serde_json::to_string(&ids).expect("snowflake list serializes"),
                row.state_json,
            ],
        )?;
        Ok(())
    }

    /// Acquires the single-process SQLite serialization boundary.
    fn connection(&self) -> BotResult<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| BotError::Other("database mutex poisoned".into()))
    }
}

/// Maps a SQLite row into the minimal persisted session representation.
fn map_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let thread: String = row.get(0)?;
    Ok(SessionRow {
        thread_id: GenericChannelId::new(parse_snowflake(&thread)?),
        session_id: row.get(1)?,
        agent_key: row.get(2)?,
        project_path: row.get(3)?,
    })
}

/// Parses a Discord snowflake stored as decimal text.
fn parse_snowflake(value: &str) -> rusqlite::Result<u64> {
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            value.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    use super::*;

    #[test]
    /// Ensures concurrent callers cannot reserve the same render turn.
    fn concurrent_turns_reserve_distinct_numbers() {
        /// Number of simultaneous turn reservations exercised by the test.
        const WORKERS: usize = 8;

        let db = Db::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let thread_id = GenericChannelId::new(1);
        db.insert_session(&SessionRow {
            thread_id,
            session_id: "session".into(),
            agent_key: "agent".into(),
            project_path: "/project".into(),
        })
        .unwrap();
        let barrier = Arc::new(Barrier::new(WORKERS));

        let mut turns = thread::scope(|scope| {
            let handles = (0..WORKERS)
                .map(|_| {
                    let db = &db;
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        db.begin_turn(thread_id).unwrap()
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        turns.sort_unstable();

        assert_eq!(turns, (1..=WORKERS as u64).collect::<Vec<_>>());
    }
}
