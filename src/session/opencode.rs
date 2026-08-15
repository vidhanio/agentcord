//! The `opencode` harness: a SQLite store reader, titles, and resume
//! arguments.
//!
//! Unlike the other harnesses, opencode persists its sessions in a SQLite
//! store (`$XDG_DATA_HOME/opencode/opencode-<channel>.db`, usually
//! `~/.local/share/opencode/opencode-stable.db`) instead of per-session
//! JSONL transcripts, so the transcript is read from the store rather than
//! from a file. Conversation text lives in `text` parts; tool calls are
//! `tool` parts whose `state` records the completion (or failure); and
//! `reasoning`, `step-*`, `patch` and other parts never carry conversation
//! and are skipped.

use std::{
    collections::HashMap,
    io::{Error as IoError, ErrorKind, Result as IoResult},
    path::{Path, PathBuf},
    time::SystemTime,
};

use rusqlite::{Connection, Error as SqlError, ErrorCode, OpenFlags};
use serde_json::Value;

use super::{
    SessionMessage, SessionRole, ToolCallId,
    common::{compact_args, tool_message},
};

/// The `opencode` harness. Sessions live in a SQLite store rather than
/// transcript files; the type owns the store access, the session titles,
/// and the resume arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opencode;

/// The store directory: `$XDG_DATA_HOME/opencode`, falling back to
/// `~/.local/share/opencode`.
fn opencode_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("opencode"))
}

/// Resolves the store file inside `data_dir`.
///
/// opencode names the store after its release channel (`opencode-stable.db`
/// for the standalone installer, `opencode.db` for the npm package, and
/// `opencode-next.db`/`opencode-canary.db` for the preview channels), so the
/// stable store is preferred and any other `opencode*.db` falls back, newest
/// first. `None` when the directory holds no store at all.
fn opencode_db_path(data_dir: &Path) -> Option<PathBuf> {
    let stable = data_dir.join("opencode-stable.db");
    if stable.is_file() {
        return Some(stable);
    }
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for entry in std::fs::read_dir(data_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("opencode") && name.ends_with(".db")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if newest
            .as_ref()
            .is_none_or(|(_, newest_time)| modified > *newest_time)
        {
            newest = Some((entry.path(), modified));
        }
    }
    newest.map(|(path, _)| path)
}

/// Opens the opencode session store read-only.
///
/// Returns [`ErrorKind::NotFound`] when no store exists yet (opencode has
/// never written one), so callers can treat a missing store like a missing
/// transcript.
fn open_db() -> IoResult<Connection> {
    let Some(data_dir) = opencode_data_dir() else {
        return Err(IoError::new(ErrorKind::NotFound, "no data directory"));
    };
    let Some(path) = opencode_db_path(&data_dir) else {
        return Err(IoError::new(
            ErrorKind::NotFound,
            format!("no opencode store under {}", data_dir.display()),
        ));
    };
    open_opencode_db_at(&path)
}

/// Opens `path` read-only, falling back to read-write when the read-only
/// open fails on an existing file. A WAL-mode store whose `-shm` file is
/// absent (e.g. after a clean opencode shutdown) cannot be opened read-only;
/// the fallback never creates the file, so a missing store still surfaces as
/// [`ErrorKind::NotFound`].
fn open_opencode_db_at(path: &Path) -> IoResult<Connection> {
    match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => Ok(conn),
        Err(_) if path.is_file() => Connection::open_with_flags(
            path,
            // Deliberately without SQLITE_OPEN_CREATE.
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .map_err(sql_error),
        Err(read_error) => Err(sql_error(read_error)),
    }
}

/// Maps a rusqlite error onto an [`IoError`], translating a missing or
/// unopenable database file into [`ErrorKind::NotFound`].
fn sql_error(error: SqlError) -> IoError {
    match error {
        SqlError::SqliteFailure(ffi_error, _) if ffi_error.code == ErrorCode::CannotOpen => {
            IoError::new(ErrorKind::NotFound, ffi_error.to_string())
        }
        other => IoError::other(other),
    }
}

/// Reads one session's transcript from the store, in conversation order.
///
/// `text` parts become user/assistant messages; `tool` parts become tool-call
/// messages whose state comes from the part's own completion status
/// (pending/running → running, completed → done, error → failed). The
/// `reasoning`, `step-*`, `patch` and `file` parts never carry conversation
/// and are skipped. An unknown `session_id` yields an empty transcript.
fn read_opencode_session(conn: &Connection, session_id: &str) -> IoResult<Vec<SessionMessage>> {
    let messages = load_messages(conn, session_id)?;
    let parts = load_parts(conn, session_id)?;
    // Pre-scan completion records: a `tool` part carries its own final state
    // (completed or error), and every call's state is known up front.
    let results = tool_results(&parts);

    let mut transcript = Vec::new();
    for (message_id, role) in messages {
        let mut text: Vec<&str> = Vec::new();
        for part in parts.get(&message_id).into_iter().flatten() {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                        text.push(part_text);
                    }
                }
                Some("tool") => {
                    if let Some(message) = tool_part(part, &results) {
                        transcript.push(message);
                    }
                }
                // reasoning, step-start, step-finish, patch, file, ...
                _ => {}
            }
        }
        let text = text.join("\n");
        if text.trim().is_empty() {
            continue;
        }
        transcript.push(SessionMessage {
            role,
            text,
            tool: None,
        });
    }
    Ok(transcript)
}

/// The messages of `session_id` in conversation order, with their roles.
fn load_messages(conn: &Connection, session_id: &str) -> IoResult<Vec<(String, SessionRole)>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, data FROM message WHERE session_id = ?1 \
             ORDER BY time_created, id",
        )
        .map_err(sql_error)?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error)?;
    let mut messages = Vec::new();
    for row in rows {
        let (id, data) = row.map_err(sql_error)?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let role = match value.get("role").and_then(Value::as_str) {
            Some("user") => SessionRole::User,
            Some("assistant") => SessionRole::Agent,
            _ => continue,
        };
        messages.push((id, role));
    }
    Ok(messages)
}

/// The parts of `session_id` grouped by message, in conversation order.
fn load_parts(conn: &Connection, session_id: &str) -> IoResult<HashMap<String, Vec<Value>>> {
    let mut stmt = conn
        .prepare(
            "SELECT message_id, data FROM part WHERE session_id = ?1 \
             ORDER BY time_created, id",
        )
        .map_err(sql_error)?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error)?;
    let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
    for row in rows {
        let (message_id, data) = row.map_err(sql_error)?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        parts.entry(message_id).or_default().push(value);
    }
    Ok(parts)
}

/// The completion records of every settled `tool` part: call id →
/// (errored, raw error text). Calls still pending or running are left out,
/// so they surface as [`super::ToolState::Running`].
fn tool_results(parts: &HashMap<String, Vec<Value>>) -> HashMap<ToolCallId, (bool, String)> {
    let mut results: HashMap<ToolCallId, (bool, String)> = HashMap::new();
    for part in parts.values().flatten() {
        if part.get("type").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(call_id) = part.get("callID").and_then(Value::as_str) else {
            continue;
        };
        let (errored, text) = match part.pointer("/state/status").and_then(Value::as_str) {
            Some("completed") => (false, String::new()),
            Some("error") => (
                true,
                part.pointer("/state/error")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            _ => continue,
        };
        results.insert(ToolCallId::from(call_id), (errored, text));
    }
    results
}

/// Builds the tool-call message for a `tool` part.
fn tool_part(
    part: &Value,
    results: &HashMap<ToolCallId, (bool, String)>,
) -> Option<SessionMessage> {
    let name = part.get("tool").and_then(Value::as_str)?;
    let call_id = part.get("callID").and_then(Value::as_str)?;
    let args = part.pointer("/state/input").map(compact_args);
    Some(tool_message(
        name.to_owned(),
        ToolCallId::from(call_id),
        args,
        results,
    ))
}

/// Reads the session's own title from the store (`session.title`); `None`
/// for an unknown session or a blank title.
#[must_use]
fn read_opencode_title(conn: &Connection, session_id: &str) -> Option<String> {
    let title = conn
        .query_row(
            "SELECT title FROM session WHERE id = ?1",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()?;
    let title = title.trim().to_owned();
    (!title.is_empty()).then_some(title)
}

impl Opencode {
    /// Reads a session's transcript from the opencode store, keyed by
    /// session id (see [`read_opencode_session`] for the semantics).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::NotFound`] when no store exists yet.
    pub fn read_session(session_id: &str) -> IoResult<Vec<SessionMessage>> {
        open_db().and_then(|conn| read_opencode_session(&conn, session_id))
    }

    /// The store's title for a session; `None` when no store exists, the
    /// session is unknown, or the title is blank.
    #[must_use]
    pub fn read_title(session_id: &str) -> Option<String> {
        open_db()
            .ok()
            .and_then(|conn| read_opencode_title(&conn, session_id))
    }

    /// The `agent.start` arguments that resume a session: opencode resumes
    /// by session id via `--session`.
    #[must_use]
    pub fn resume_args(session: &str) -> Vec<String> {
        vec!["--session".into(), session.to_owned()]
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::ErrorKind,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, SystemTime},
    };

    use rusqlite::Connection;

    use super::{
        super::{SessionRole, ToolCallId, ToolState},
        open_opencode_db_at, opencode_db_path, read_opencode_session, read_opencode_title,
    };

    /// A unique scratch directory per test, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "herdcord-opencode-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A store with the `session`/`message`/`part` tables (the subset of
    /// opencode's schema these readers touch).
    fn fixture_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, slug TEXT NOT NULL,
                directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_session(conn: &Connection, id: &str, title: &str) {
        conn.execute(
            "INSERT INTO session (id, project_id, slug, directory, title, version, \
             time_created, time_updated) VALUES (?1, 'project', 'slug', '/tmp', ?2, '1.2.27', 1, 1)",
            rusqlite::params![id, title],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, id: &str, session: &str, created: i64, role: &str) {
        let data = format!(r#"{{"role":"{role}","time":{{"created":{created}}}}}"#);
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?3, ?4)",
            rusqlite::params![id, session, created, data],
        )
        .unwrap();
    }

    fn insert_part(
        conn: &Connection,
        id: &str,
        message: &str,
        session: &str,
        created: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            rusqlite::params![id, message, session, created, data],
        )
        .unwrap();
    }

    #[test]
    fn opencode_session_reads_text_and_tools_in_order() {
        let conn = fixture_db();
        let session = "ses_fixture";
        insert_message(&conn, "m1", session, 1, "user");
        insert_part(
            &conn,
            "p1",
            "m1",
            session,
            1,
            r#"{"type":"text","text":"create a bot"}"#,
        );
        insert_message(&conn, "m2", session, 2, "assistant");
        insert_part(&conn, "p2", "m2", session, 2, r#"{"type":"step-start"}"#);
        insert_part(
            &conn,
            "p3",
            "m2",
            session,
            3,
            r#"{"type":"reasoning","text":"hidden thinking"}"#,
        );
        insert_part(
            &conn,
            "p4",
            "m2",
            session,
            4,
            r#"{"type":"tool","callID":"call_1","tool":"read","state":{"status":"completed","input":{"path":"x"}}}"#,
        );
        insert_part(
            &conn,
            "p5",
            "m2",
            session,
            5,
            r#"{"type":"tool","callID":"call_2","tool":"bash","state":{"status":"error","input":{"command":"ls"},"error":"boom"}}"#,
        );
        insert_part(
            &conn,
            "p6",
            "m2",
            session,
            6,
            r#"{"type":"text","text":"done"}"#,
        );
        insert_message(&conn, "m3", session, 3, "user");
        insert_part(
            &conn,
            "p7",
            "m3",
            session,
            7,
            r#"{"type":"text","text":"thanks"}"#,
        );

        let messages = read_opencode_session(&conn, session).unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role, SessionRole::User);
        assert_eq!(messages[0].text, "create a bot");
        assert!(messages[0].tool.is_none());

        let read = messages[1].tool.as_ref().unwrap();
        assert_eq!(messages[1].role, SessionRole::Tool);
        assert_eq!(read.name, "read");
        assert_eq!(read.call_id, ToolCallId::from("call_1"));
        assert_eq!(read.state, ToolState::Done);
        assert_eq!(read.args.as_deref(), Some(r#"{"path":"x"}"#));

        let bash = messages[2].tool.as_ref().unwrap();
        assert_eq!(bash.name, "bash");
        assert_eq!(bash.state, ToolState::Failed);
        assert_eq!(bash.error.as_deref(), Some("boom"));

        assert_eq!(messages[3].role, SessionRole::Agent);
        assert_eq!(messages[3].text, "done");
        assert_eq!(messages[4].role, SessionRole::User);
        assert_eq!(messages[4].text, "thanks");
        assert!(
            messages
                .iter()
                .all(|message| message.text != "hidden thinking")
        );
    }

    #[test]
    fn opencode_pending_tool_is_running() {
        let conn = fixture_db();
        let session = "ses_pending";
        insert_message(&conn, "m1", session, 1, "assistant");
        insert_part(
            &conn,
            "p1",
            "m1",
            session,
            1,
            r#"{"type":"tool","callID":"call_1","tool":"grep","state":{"status":"pending","input":{"pattern":"x"}}}"#,
        );
        insert_part(
            &conn,
            "p2",
            "m1",
            session,
            2,
            r#"{"type":"text","text":"still working"}"#,
        );

        let messages = read_opencode_session(&conn, session).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tool.as_ref().unwrap().state, ToolState::Running);
        assert_eq!(messages[0].tool.as_ref().unwrap().error, None);
        assert_eq!(messages[1].role, SessionRole::Agent);
        assert_eq!(messages[1].text, "still working");
    }

    #[test]
    fn opencode_message_without_text_parts_is_skipped() {
        let conn = fixture_db();
        let session = "ses_file_only";
        insert_message(&conn, "m1", session, 1, "user");
        insert_part(
            &conn,
            "p1",
            "m1",
            session,
            1,
            r#"{"type":"file","file":{"path":"a.png"}}"#,
        );
        assert_eq!(read_opencode_session(&conn, session).unwrap(), Vec::new());
    }

    #[test]
    fn opencode_unknown_session_is_empty() {
        let conn = fixture_db();
        assert_eq!(
            read_opencode_session(&conn, "ses_missing").unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn opencode_title_reads_store_title() {
        let conn = fixture_db();
        insert_session(&conn, "ses_t", "  A real task  ");
        insert_session(&conn, "ses_blank", "   ");
        assert_eq!(
            read_opencode_title(&conn, "ses_t").as_deref(),
            Some("A real task")
        );
        assert_eq!(read_opencode_title(&conn, "ses_blank"), None);
        assert_eq!(read_opencode_title(&conn, "ses_missing"), None);
    }

    #[test]
    fn opencode_db_path_prefers_stable() {
        let dir = TempDir::new("prefer-stable");
        fs::write(dir.path().join("opencode.db"), b"").unwrap();
        fs::write(dir.path().join("opencode-stable.db"), b"").unwrap();
        assert_eq!(
            opencode_db_path(dir.path()),
            Some(dir.path().join("opencode-stable.db"))
        );
    }

    #[test]
    fn opencode_db_path_picks_newest_channel_db() {
        let dir = TempDir::new("newest");
        let plain = dir.path().join("opencode.db");
        let next = dir.path().join("opencode-next.db");
        fs::write(&plain, b"").unwrap();
        fs::write(&next, b"").unwrap();
        let old = SystemTime::UNIX_EPOCH;
        let new = old + Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&plain)
            .unwrap()
            .set_modified(old)
            .unwrap();
        fs::File::options()
            .write(true)
            .open(&next)
            .unwrap()
            .set_modified(new)
            .unwrap();
        assert_eq!(opencode_db_path(dir.path()), Some(next));
    }

    #[test]
    fn opencode_db_path_none_without_store() {
        let dir = TempDir::new("empty");
        assert_eq!(opencode_db_path(dir.path()), None);
    }

    #[test]
    fn open_opencode_db_at_missing_file_is_not_found() {
        let dir = TempDir::new("missing");
        let err = open_opencode_db_at(&dir.path().join("opencode.db")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn open_opencode_db_at_reads_wal_store_after_clean_close() {
        let dir = TempDir::new("wal");
        let path = dir.path().join("opencode.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE t (x INTEGER);
                 INSERT INTO t VALUES (1), (2);",
            )
            .unwrap();
        }
        // A clean close checkpoints WAL and removes the -wal/-shm files, so
        // the store can only be opened through the read-write fallback.
        assert!(!path.with_extension("db-wal").exists());
        let conn = open_opencode_db_at(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}
