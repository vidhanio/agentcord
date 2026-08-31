//! Durable session bindings and Discord render projections.
#![allow(
    clippy::used_underscore_items,
    reason = "Toasty's unique-index derive generates private underscore helpers"
)]

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::SessionId;
use serenity::all::{GenericChannelId, MessageId};
use tracing::info;

use crate::{BotError, BotResult, config::AgentKey};

/// The durable information required to restore one ACP session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRow {
    /// Discord forum thread representing the session.
    pub thread_id: GenericChannelId,
    /// Configured agent used to open the ACP session.
    pub agent_key: AgentKey,
    /// Agent-owned opaque ACP session identifier.
    pub session_id: SessionId,
    /// Working directory used by the session.
    pub project_path: PathBuf,
}

/// The complete Discord projection for one logical ACP source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderProjection {
    /// Discord forum thread containing the projection.
    pub thread_id: GenericChannelId,
    /// Kind of logical item, such as `turn` or `tool_call`.
    pub source_kind: String,
    /// Opaque identifier within the source kind.
    pub source_id: String,
    /// Renderer-owned accumulated state.
    pub state_json: String,
    /// Discord messages representing the source, in display order.
    pub message_ids: Vec<MessageId>,
}

#[derive(Debug, toasty::Model)]
#[table = "sessions"]
#[unique(agent, acp)]
struct Session {
    #[key]
    /// Discord thread ID stored as text for SQLite portability.
    thread_id: String,
    #[column("agent_key")]
    /// Configured ACP executable key.
    agent: String,
    #[column("acp_session_id")]
    /// Opaque ACP session ID.
    acp: String,
    /// Absolute working directory used by ACP.
    project_path: String,
}

#[derive(Debug, toasty::Model)]
#[table = "render_sources"]
#[key(thread_id, source_kind, source_id)]
struct RenderSource {
    /// Owning Discord thread ID.
    thread_id: String,
    /// Logical source category such as an agent message.
    source_kind: String,
    /// Stable ID within the source category.
    source_id: String,
    /// Renderer-owned serialized state.
    state_json: String,
    #[belongs_to(key = thread_id, references = thread_id)]
    /// Session binding that owns this source.
    session: toasty::Deferred<Session>,
}

#[derive(Debug, toasty::Model)]
#[table = "render_messages"]
#[key(thread_id, source_kind, source_id, position)]
struct RenderMessage {
    /// Owning Discord thread ID.
    thread_id: String,
    /// Logical source category.
    source_kind: String,
    /// Stable ID within the source category.
    source_id: String,
    /// Display order within the source.
    position: i64,
    #[unique]
    #[column("message_id")]
    /// Discord message ID stored as text.
    message: String,
    #[belongs_to(
        key = [thread_id, source_kind, source_id],
        references = [thread_id, source_kind, source_id]
    )]
    /// Render source that owns this message.
    source: toasty::Deferred<RenderSource>,
}

/// Toasty-backed access to Agentcord's SQLite state.
pub struct Db {
    /// Toasty database handle shared by asynchronous operations.
    inner: toasty::Db,
}

impl Db {
    /// Opens the state database and creates its schema for a new file.
    pub async fn open(path: &Path) -> BotResult<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let create_schema = !path.exists();
        let path = path.to_str().ok_or_else(|| BotError::DatabasePathNotUtf8 {
            path: path.to_owned(),
        })?;
        let db = Self::connect(&format!("sqlite:{path}"), create_schema).await?;
        info!(path, create_schema, "state database ready");
        Ok(db)
    }

    /// Connects to a database and optionally creates its initial schema.
    async fn connect(url: &str, create_schema: bool) -> BotResult<Self> {
        let inner = toasty::Db::builder()
            .models(toasty::models!(Session, RenderSource, RenderMessage))
            .connect(url)
            .await?;
        if create_schema {
            inner.push_schema().await?;
            info!("state database schema created");
        }
        Ok(Self { inner })
    }

    /// Inserts a durable Discord-to-ACP session binding.
    pub async fn insert_session(&self, row: &SessionRow) -> BotResult {
        if !row.project_path.is_absolute() {
            return Err(BotError::RelativeProjectPath {
                path: row.project_path.clone(),
            });
        }
        let project_path =
            row.project_path
                .to_str()
                .ok_or_else(|| BotError::ProjectPathNotUtf8 {
                    path: row.project_path.clone(),
                })?;
        let mut db = self.inner.clone();
        toasty::create!(Session {
            thread_id: row.thread_id.to_string(),
            agent: row.agent_key.as_ref(),
            acp: row.session_id.0.as_ref(),
            project_path,
        })
        .exec(&mut db)
        .await?;
        info!(
            thread = ?row.thread_id,
            agent = %row.agent_key,
            session = %row.session_id,
            "stored session binding"
        );
        Ok(())
    }

    /// Finds the session bound to a Discord thread.
    pub async fn session(&self, thread_id: GenericChannelId) -> BotResult<Option<SessionRow>> {
        let mut db = self.inner.clone();
        let session = Session::filter_by_thread_id(thread_id.to_string())
            .first()
            .exec(&mut db)
            .await?;
        let session = session.map(SessionRow::try_from).transpose()?;
        Ok(session)
    }

    /// Lists every persisted session.
    pub async fn sessions(&self) -> BotResult<Vec<SessionRow>> {
        let mut db = self.inner.clone();
        let sessions = Session::all()
            .exec(&mut db)
            .await?
            .into_iter()
            .map(SessionRow::try_from)
            .collect::<BotResult<Vec<_>>>()?;
        Ok(sessions)
    }

    /// Finds a Discord binding by its configured agent and ACP session id.
    pub async fn session_by_agent(
        &self,
        agent_key: &AgentKey,
        session_id: &SessionId,
    ) -> BotResult<Option<SessionRow>> {
        let mut db = self.inner.clone();
        let session = Session::filter(
            Session::fields()
                .agent()
                .eq(agent_key.as_ref())
                .and(Session::fields().acp().eq(session_id.0.as_ref())),
        )
        .first()
        .exec(&mut db)
        .await?
        .map(SessionRow::try_from)
        .transpose()?;
        Ok(session)
    }

    /// Deletes a session and all of its render projections.
    pub async fn delete_session(&self, thread_id: GenericChannelId) -> BotResult {
        let thread_id = thread_id.to_string();
        let mut db = self.inner.clone();
        let mut transaction = db.transaction().await?;
        RenderMessage::filter(RenderMessage::fields().thread_id().eq(&thread_id))
            .delete()
            .exec(&mut transaction)
            .await?;
        RenderSource::filter(RenderSource::fields().thread_id().eq(&thread_id))
            .delete()
            .exec(&mut transaction)
            .await?;
        Session::filter_by_thread_id(&thread_id)
            .delete()
            .exec(&mut transaction)
            .await?;
        transaction.commit().await?;
        info!(thread = %thread_id, "deleted session binding and projections");
        Ok(())
    }

    /// Atomically replaces one logical source's state and ordered messages.
    pub async fn replace_projection(&self, projection: &RenderProjection) -> BotResult {
        let thread_id = projection.thread_id.to_string();
        let mut db = self.inner.clone();
        let mut transaction = db.transaction().await?;
        let source = source_filter(&thread_id, &projection.source_kind, &projection.source_id);
        RenderMessage::filter(message_source_filter(
            &thread_id,
            &projection.source_kind,
            &projection.source_id,
        ))
        .delete()
        .exec(&mut transaction)
        .await?;
        RenderSource::filter(source)
            .delete()
            .exec(&mut transaction)
            .await?;
        toasty::create!(RenderSource {
            thread_id: thread_id.as_str(),
            source_kind: projection.source_kind.as_str(),
            source_id: projection.source_id.as_str(),
            state_json: projection.state_json.as_str(),
        })
        .exec(&mut transaction)
        .await?;
        for (position, message_id) in projection.message_ids.iter().enumerate() {
            let position = i64::try_from(position).map_err(|_| BotError::ProjectionTooLarge)?;
            toasty::create!(RenderMessage {
                thread_id: thread_id.as_str(),
                source_kind: projection.source_kind.as_str(),
                source_id: projection.source_id.as_str(),
                position,
                message: message_id.to_string(),
            })
            .exec(&mut transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Loads one logical source and its ordered Discord messages.
    pub async fn projection(
        &self,
        thread_id: GenericChannelId,
        source_kind: &str,
        source_id: &str,
    ) -> BotResult<Option<RenderProjection>> {
        let thread_id_text = thread_id.to_string();
        let mut db = self.inner.clone();
        let source = RenderSource::filter(source_filter(&thread_id_text, source_kind, source_id))
            .first()
            .exec(&mut db)
            .await?;
        let Some(source) = source else {
            return Ok(None);
        };
        let messages = RenderMessage::filter(message_source_filter(
            &thread_id_text,
            source_kind,
            source_id,
        ))
        .order_by(RenderMessage::fields().position().asc())
        .exec(&mut db)
        .await?;
        let message_ids = messages
            .into_iter()
            .map(|message| parse_id(&message.message))
            .collect::<BotResult<_>>()?;

        let projection = Some(RenderProjection {
            thread_id,
            source_kind: source.source_kind,
            source_id: source.source_id,
            state_json: source.state_json,
            message_ids,
        });
        Ok(projection)
    }

    #[cfg(test)]
    /// Opens an isolated in-memory database for persistence tests.
    async fn in_memory() -> BotResult<Self> {
        Self::connect("sqlite::memory:", true).await
    }
}

impl std::fmt::Debug for Db {
    /// Omits internal database handles from debug output.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Db").finish_non_exhaustive()
    }
}

impl TryFrom<Session> for SessionRow {
    type Error = BotError;

    /// Validates and converts a Toasty model into the public row type.
    fn try_from(session: Session) -> Result<Self, Self::Error> {
        let project_path = PathBuf::from(session.project_path);
        if !project_path.is_absolute() {
            return Err(BotError::RelativeProjectPath { path: project_path });
        }
        Ok(Self {
            thread_id: parse_id(&session.thread_id)?,
            agent_key: AgentKey::new(session.agent),
            session_id: SessionId::new(session.acp),
            project_path,
        })
    }
}

/// Builds the predicate for one stored render source.
fn source_filter(thread_id: &str, source_kind: &str, source_id: &str) -> toasty::stmt::Expr<bool> {
    RenderSource::fields()
        .thread_id()
        .eq(thread_id)
        .and(RenderSource::fields().source_kind().eq(source_kind))
        .and(RenderSource::fields().source_id().eq(source_id))
}

/// Builds the predicate for messages belonging to one render source.
fn message_source_filter(
    thread_id: &str,
    source_kind: &str,
    source_id: &str,
) -> toasty::stmt::Expr<bool> {
    RenderMessage::fields()
        .thread_id()
        .eq(thread_id)
        .and(RenderMessage::fields().source_kind().eq(source_kind))
        .and(RenderMessage::fields().source_id().eq(source_id))
}

/// Parses a persisted Discord snowflake into the requested ID type.
fn parse_id<T>(value: &str) -> BotResult<T>
where
    T: From<u64>,
{
    value
        .parse::<u64>()
        .map(T::from)
        .map_err(|source| BotError::InvalidStoredDiscordId {
            value: value.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use agent_client_protocol::schema::v1::SessionId;
    use serenity::all::{GenericChannelId, MessageId};

    use super::{Db, RenderProjection, SessionRow};
    use crate::config::AgentKey;

    /// Builds a valid test session for one synthetic Discord thread.
    fn session(thread: u64) -> SessionRow {
        SessionRow {
            thread_id: GenericChannelId::new(thread),
            agent_key: AgentKey::new("example"),
            session_id: SessionId::new(format!("session-{thread}")),
            project_path: PathBuf::from("/work/project"),
        }
    }

    /// Verifies a new database can be reopened with the same schema.
    #[tokio::test]
    async fn opens_a_new_file_and_reuses_its_schema() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agentcord-toasty-{}-{suffix}.sqlite3",
            std::process::id()
        ));

        let db = Db::open(&path).await.unwrap();
        db.insert_session(&session(5)).await.unwrap();
        drop(db);

        let reopened = Db::open(&path).await.unwrap();
        assert!(
            reopened
                .session(GenericChannelId::new(5))
                .await
                .unwrap()
                .is_some()
        );
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }

    /// Verifies session persistence and the unique agent/session constraint.
    #[tokio::test]
    async fn restores_sessions_and_enforces_agent_session_uniqueness() {
        let db = Db::in_memory().await.unwrap();
        let first = session(10);
        db.insert_session(&first).await.unwrap();
        assert_eq!(
            db.session(first.thread_id).await.unwrap(),
            Some(first.clone())
        );
        assert_eq!(db.sessions().await.unwrap(), vec![first.clone()]);

        let duplicate = SessionRow {
            thread_id: GenericChannelId::new(11),
            ..first
        };
        assert!(db.insert_session(&duplicate).await.is_err());
    }

    /// Verifies projection replacement preserves state and message order.
    #[tokio::test]
    async fn replaces_projection_state_and_message_order_atomically() {
        let db = Db::in_memory().await.unwrap();
        let session = session(20);
        db.insert_session(&session).await.unwrap();
        let mut projection = RenderProjection {
            thread_id: session.thread_id,
            source_kind: "turn".into(),
            source_id: "1".into(),
            state_json: r#"{"text":"one"}"#.into(),
            message_ids: vec![MessageId::new(2), MessageId::new(1)],
        };
        db.replace_projection(&projection).await.unwrap();
        assert_eq!(
            db.projection(session.thread_id, "turn", "1").await.unwrap(),
            Some(projection.clone())
        );

        projection.state_json = r#"{"text":"two"}"#.into();
        projection.message_ids = vec![MessageId::new(3)];
        db.replace_projection(&projection).await.unwrap();
        assert_eq!(
            db.projection(session.thread_id, "turn", "1").await.unwrap(),
            Some(projection)
        );
    }

    /// Verifies deleting a session also deletes its projections.
    #[tokio::test]
    async fn deleting_session_removes_its_projections() {
        let db = Db::in_memory().await.unwrap();
        let session = session(30);
        db.insert_session(&session).await.unwrap();
        db.replace_projection(&RenderProjection {
            thread_id: session.thread_id,
            source_kind: "tool_call".into(),
            source_id: "call-1".into(),
            state_json: "{}".into(),
            message_ids: vec![MessageId::new(4)],
        })
        .await
        .unwrap();

        db.delete_session(session.thread_id).await.unwrap();
        assert!(
            db.projection(session.thread_id, "tool_call", "call-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Verifies relative project paths cannot enter durable state.
    #[tokio::test]
    async fn rejects_relative_project_paths() {
        let db = Db::in_memory().await.unwrap();
        let mut row = session(40);
        row.project_path = PathBuf::from("relative/project");
        assert!(db.insert_session(&row).await.is_err());
    }
}
