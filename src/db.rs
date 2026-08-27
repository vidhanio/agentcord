use std::{path::Path, sync::Arc};

use rusqlite::{Connection, OptionalExtension, params};
use serenity::all::{GenericChannelId, MessageId};

use crate::{BotError, BotResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    Active,
    Restorable,
    Unavailable,
}

impl Availability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Restorable => "restorable",
            Self::Unavailable => "unavailable",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "active" => Self::Active,
            "restorable" => Self::Restorable,
            _ => Self::Unavailable,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionRow {
    pub thread_id: GenericChannelId,
    pub starter_message_id: MessageId,
    pub session_id: String,
    pub agent_key: String,
    pub project_path: String,
    pub project_label: String,
    pub title: Option<String>,
    pub protocol_version: String,
    pub capabilities_json: String,
    pub restorable: bool,
    pub availability: Availability,
    pub turn: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RenderRow {
    pub source_key: String,
    pub kind: String,
    pub discord_message_ids: Vec<MessageId>,
    pub state_json: String,
}

#[derive(Clone, Debug)]
pub struct Db {
    connection: Arc<std::sync::Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> BotResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    fn from_connection(connection: Connection) -> BotResult<Self> {
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS sessions (
               thread_id TEXT PRIMARY KEY,
               starter_message_id TEXT NOT NULL,
               session_id TEXT NOT NULL,
               agent_key TEXT NOT NULL,
               project_path TEXT NOT NULL,
               project_label TEXT NOT NULL,
               title TEXT,
               protocol_version TEXT NOT NULL,
               capabilities_json TEXT NOT NULL,
               restorable INTEGER NOT NULL,
               availability TEXT NOT NULL,
               turn INTEGER NOT NULL DEFAULT 0,
               last_error TEXT,
               UNIQUE(agent_key, session_id)
             );
             CREATE TABLE IF NOT EXISTS renders (
               thread_id TEXT NOT NULL REFERENCES sessions(thread_id) ON DELETE CASCADE,
               source_key TEXT NOT NULL,
               kind TEXT NOT NULL,
               discord_message_ids TEXT NOT NULL,
               state_json TEXT NOT NULL,
               PRIMARY KEY(thread_id, source_key)
             );",
        )?;
        Ok(Self {
            connection: Arc::new(std::sync::Mutex::new(connection)),
        })
    }

    pub fn insert_session(&self, row: &SessionRow) -> BotResult {
        self.connection()?.execute(
            "INSERT INTO sessions (
               thread_id, starter_message_id, session_id, agent_key, project_path,
               project_label, title, protocol_version, capabilities_json,
               restorable, availability, turn, last_error
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                row.thread_id.to_string(),
                row.starter_message_id.to_string(),
                row.session_id,
                row.agent_key,
                row.project_path,
                row.project_label,
                row.title,
                row.protocol_version,
                row.capabilities_json,
                row.restorable,
                row.availability.as_str(),
                i64::try_from(row.turn).unwrap_or(i64::MAX),
                row.last_error,
            ],
        )?;
        Ok(())
    }

    pub fn session(&self, thread_id: GenericChannelId) -> BotResult<Option<SessionRow>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT thread_id, starter_message_id, session_id, agent_key,
                        project_path, project_label, title, protocol_version,
                        capabilities_json, restorable, availability, turn, last_error
                 FROM sessions WHERE thread_id = ?1",
                [thread_id.to_string()],
                map_session,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn sessions(&self) -> BotResult<Vec<SessionRow>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT thread_id, starter_message_id, session_id, agent_key,
                    project_path, project_label, title, protocol_version,
                    capabilities_json, restorable, availability, turn, last_error
             FROM sessions ORDER BY rowid",
        )?;
        let rows = statement.query_map([], map_session)?;
        let sessions = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        drop(connection);
        Ok(sessions)
    }

    pub fn set_availability(
        &self,
        thread_id: GenericChannelId,
        availability: Availability,
        error: Option<&str>,
    ) -> BotResult {
        self.connection()?.execute(
            "UPDATE sessions SET availability = ?2, last_error = ?3 WHERE thread_id = ?1",
            params![thread_id.to_string(), availability.as_str(), error],
        )?;
        Ok(())
    }

    pub fn set_title(&self, thread_id: GenericChannelId, title: Option<&str>) -> BotResult {
        self.connection()?.execute(
            "UPDATE sessions SET title = ?2 WHERE thread_id = ?1",
            params![thread_id.to_string(), title],
        )?;
        Ok(())
    }

    pub fn begin_turn(&self, thread_id: GenericChannelId) -> BotResult<u64> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE sessions SET turn = turn + 1 WHERE thread_id = ?1",
            [thread_id.to_string()],
        )?;
        let turn: i64 = connection.query_row(
            "SELECT turn FROM sessions WHERE thread_id = ?1",
            [thread_id.to_string()],
            |row| row.get(0),
        )?;
        drop(connection);
        Ok(u64::try_from(turn).unwrap_or_default())
    }

    pub fn render(
        &self,
        thread_id: GenericChannelId,
        source_key: &str,
    ) -> BotResult<Option<RenderRow>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT source_key, kind, discord_message_ids, state_json
                 FROM renders WHERE thread_id = ?1 AND source_key = ?2",
                params![thread_id.to_string(), source_key],
                |row| {
                    let ids: String = row.get(2)?;
                    let ids = serde_json::from_str::<Vec<u64>>(&ids)
                        .unwrap_or_default()
                        .into_iter()
                        .map(MessageId::new)
                        .collect();
                    Ok(RenderRow {
                        source_key: row.get(0)?,
                        kind: row.get(1)?,
                        discord_message_ids: ids,
                        state_json: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn upsert_render(&self, thread_id: GenericChannelId, row: &RenderRow) -> BotResult {
        let ids = row
            .discord_message_ids
            .iter()
            .map(|id| id.get())
            .collect::<Vec<_>>();
        self.connection()?.execute(
            "INSERT INTO renders (thread_id, source_key, kind, discord_message_ids, state_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id, source_key) DO UPDATE SET
               kind = excluded.kind,
               discord_message_ids = excluded.discord_message_ids,
               state_json = excluded.state_json",
            params![
                thread_id.to_string(),
                row.source_key,
                row.kind,
                serde_json::to_string(&ids).expect("snowflake list serializes"),
                row.state_json,
            ],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, thread_id: GenericChannelId) -> BotResult {
        self.connection()?.execute(
            "DELETE FROM sessions WHERE thread_id = ?1",
            [thread_id.to_string()],
        )?;
        Ok(())
    }

    fn connection(&self) -> BotResult<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| BotError::Other("database mutex poisoned".into()))
    }
}

fn map_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let thread: String = row.get(0)?;
    let starter: String = row.get(1)?;
    let availability: String = row.get(10)?;
    Ok(SessionRow {
        thread_id: GenericChannelId::new(parse_snowflake(&thread)?),
        starter_message_id: MessageId::new(parse_snowflake(&starter)?),
        session_id: row.get(2)?,
        agent_key: row.get(3)?,
        project_path: row.get(4)?,
        project_label: row.get(5)?,
        title: row.get(6)?,
        protocol_version: row.get(7)?,
        capabilities_json: row.get(8)?,
        restorable: row.get(9)?,
        availability: Availability::parse(&availability),
        turn: u64::try_from(row.get::<_, i64>(11)?).unwrap_or_default(),
        last_error: row.get(12)?,
    })
}

fn parse_snowflake(value: &str) -> rusqlite::Result<u64> {
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            value.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}
