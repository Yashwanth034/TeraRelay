use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager};

pub type DbConnection = Arc<Mutex<sqlite::Connection>>;

/// Maximum number of retry attempts for database initialization
const MAX_DB_INIT_RETRIES: u32 = 5;

fn configure_database_durability(conn: &sqlite::Connection) -> Result<(), String> {
    // WAL avoids DELETE-journal metadata churn on every small filesystem
    // transaction while FULL keeps SQLite's strongest ordinary sync policy.
    // This is a latency optimization, not a durability downgrade.
    conn.execute("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .map_err(|e: sqlite::Error| format!("Failed to configure SQLite durability: {e}"))?;

    let mut journal = conn
        .prepare("PRAGMA journal_mode")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if journal.next().map_err(|e: sqlite::Error| e.to_string())? != sqlite::State::Row
        || !journal
            .read::<String, _>(0)
            .map_err(|e| e.to_string())?
            .eq_ignore_ascii_case("wal")
    {
        return Err("SQLite refused WAL journal mode".to_string());
    }
    drop(journal);

    let mut synchronous = conn
        .prepare("PRAGMA synchronous")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if synchronous
        .next()
        .map_err(|e: sqlite::Error| e.to_string())?
        != sqlite::State::Row
        || synchronous.read::<i64, _>(0).map_err(|e| e.to_string())? != 2
    {
        return Err("SQLite refused FULL synchronous durability".to_string());
    }
    Ok(())
}

/// Migrate v0.2.2's invalid global message-ID uniqueness without touching
/// existing chunk rows or changing any Telegram/Drive identity.
fn migrate_chunk_message_index(conn: &sqlite::Connection) -> Result<(), sqlite::Error> {
    // A message ID identifies a message only inside its Telegram channel.
    // A file's own chunk uniqueness remains PRIMARY KEY(file_id, chunk_index),
    // and validate_manifest rejects repeated message IDs within one file.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_logical_file_chunk_lookup
             ON logical_file_chunks(file_id, message_id);
         DROP INDEX IF EXISTS idx_logical_file_chunk_message;",
    )
}

pub fn init_db(app: &AppHandle) -> Result<DbConnection, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let db_path = dir.join("shares.db");

    // Retry opening the database with exponential backoff.
    // SQLite may report "database is locked" if another process or a stale
    // wal/shm journal hasn't been cleaned up yet (e.g., after a crash).
    let conn = {
        let mut last_err = String::new();
        let mut opened = None;
        for attempt in 0..MAX_DB_INIT_RETRIES {
            match sqlite::open(&db_path) {
                Ok(c) => {
                    opened = Some(c);
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    if attempt < MAX_DB_INIT_RETRIES - 1 {
                        let wait_ms = 100 * 2u64.pow(attempt);
                        log::warn!(
                            "Failed to open SQLite database (attempt {}/{}): {}. Retrying in {}ms...",
                            attempt + 1, MAX_DB_INIT_RETRIES, last_err, wait_ms
                        );
                        std::thread::sleep(Duration::from_millis(wait_ms));
                    }
                }
            }
        }
        opened.ok_or_else(|| {
            format!(
                "Failed to open SQLite database after {} attempts: {}",
                MAX_DB_INIT_RETRIES, last_err
            )
        })?
    };

    configure_database_durability(&conn)?;

    // Run migration (also with retry for locked-database scenarios)
    {
        let mut last_err = String::new();
        for attempt in 0..MAX_DB_INIT_RETRIES {
            match conn.execute(
                "CREATE TABLE IF NOT EXISTS shared_links (
                    id TEXT PRIMARY KEY,
                    folder_id INTEGER,
                    message_id INTEGER NOT NULL,
                    file_name TEXT NOT NULL,
                    file_size INTEGER NOT NULL DEFAULT 0,
                    password_hash TEXT,
                    password_salt TEXT,
                    expires_at INTEGER,
                    revoked INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS groups (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    color_hex TEXT DEFAULT '#3B82F6',
                    display_order INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS folder_metadata (
                    channel_id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    username TEXT,
                    is_public INTEGER NOT NULL DEFAULT 0,
                    display_order INTEGER NOT NULL DEFAULT 0,
                    group_id INTEGER,
                    FOREIGN KEY(group_id) REFERENCES groups(id) ON DELETE SET NULL
                );
                CREATE TABLE IF NOT EXISTS logical_channels (
                    logical_id TEXT PRIMARY KEY,
                    backing_channel_id INTEGER NOT NULL UNIQUE,
                    name TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'owner',
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    joined_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS logical_files (
                    file_id TEXT PRIMARY KEY,
                    logical_channel_id TEXT NOT NULL,
                    first_message_id INTEGER NOT NULL,
                    display_name TEXT NOT NULL,
                    total_size INTEGER NOT NULL,
                    mime_type TEXT,
                    chunk_count INTEGER NOT NULL DEFAULT 1,
                    whole_sha256 TEXT,
                    storage_version INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL,
                    status TEXT NOT NULL DEFAULT 'complete',
                    FOREIGN KEY(logical_channel_id) REFERENCES logical_channels(logical_id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_logical_files_channel
                    ON logical_files(logical_channel_id, created_at DESC);
                CREATE TABLE IF NOT EXISTS logical_file_chunks (
                    file_id TEXT NOT NULL,
                    chunk_index INTEGER NOT NULL,
                    message_id INTEGER NOT NULL,
                    chunk_size INTEGER NOT NULL,
                    sha256 TEXT,
                    PRIMARY KEY(file_id, chunk_index),
                    FOREIGN KEY(file_id) REFERENCES logical_files(file_id) ON DELETE CASCADE
                );
                INSERT OR IGNORE INTO logical_channels (
                    logical_id,
                    backing_channel_id,
                    name,
                    role,
                    storage_version,
                    created_at,
                    joined_at
                )
                SELECT
                    lower(hex(randomblob(16))),
                    channel_id,
                    name,
                    'owner',
                    1,
                    CAST(strftime('%s','now') AS INTEGER),
                    CAST(strftime('%s','now') AS INTEGER)
                FROM folder_metadata;",
            ).and_then(|_| migrate_chunk_message_index(&conn)) {
                Ok(_) => {
                    last_err.clear();
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    if attempt < MAX_DB_INIT_RETRIES - 1 {
                        let wait_ms = 100 * 2u64.pow(attempt);
                        log::warn!(
                            "Failed to run SQLite migration (attempt {}/{}): {}. Retrying in {}ms...",
                            attempt + 1, MAX_DB_INIT_RETRIES, last_err, wait_ms
                        );
                        std::thread::sleep(Duration::from_millis(wait_ms));
                    }
                }
            }
        }
        if !last_err.is_empty() {
            return Err(format!(
                "Failed to run SQLite migration after {} attempts: {}",
                MAX_DB_INIT_RETRIES, last_err
            ));
        }
    }

    log::info!("SQLite database initialized successfully using sqlite crate.");
    Ok(Arc::new(Mutex::new(conn)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_global_message_id_index_migrates_without_losing_chunks() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE logical_file_chunks (
                file_id TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                message_id INTEGER NOT NULL,
                chunk_size INTEGER NOT NULL,
                sha256 TEXT,
                PRIMARY KEY(file_id, chunk_index)
            );
            CREATE UNIQUE INDEX idx_logical_file_chunk_message
                ON logical_file_chunks(message_id);
            INSERT INTO logical_file_chunks VALUES ('old-channel-file', 1, 7, 100, NULL);",
        )
        .unwrap();

        // The previous v0.2.2 index prevents another channel using ID 7.
        assert!(conn
            .execute(
                "INSERT INTO logical_file_chunks VALUES ('new-channel-file', 1, 7, 200, NULL);"
            )
            .is_err());

        migrate_chunk_message_index(&conn).unwrap();
        migrate_chunk_message_index(&conn).unwrap(); // safe on subsequent starts
        conn.execute(
            "INSERT INTO logical_file_chunks VALUES ('new-channel-file', 1, 7, 200, NULL);",
        )
        .unwrap();

        let mut rows = conn.prepare(
            "SELECT file_id, chunk_size FROM logical_file_chunks WHERE message_id=7 ORDER BY file_id"
        ).unwrap();
        assert_eq!(rows.next().unwrap(), sqlite::State::Row);
        assert_eq!(rows.read::<String, _>(0).unwrap(), "new-channel-file");
        assert_eq!(rows.read::<i64, _>(1).unwrap(), 200);
        assert_eq!(rows.next().unwrap(), sqlite::State::Row);
        assert_eq!(rows.read::<String, _>(0).unwrap(), "old-channel-file");
        assert_eq!(rows.read::<i64, _>(1).unwrap(), 100);
        assert_eq!(rows.next().unwrap(), sqlite::State::Done);
        // A file cannot index the same chunk twice.
        assert!(conn
            .execute(
                "INSERT INTO logical_file_chunks VALUES ('new-channel-file', 1, 8, 200, NULL);"
            )
            .is_err());
    }

    #[test]
    fn database_durability_is_wal_full() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-db-durability-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("shares.db");
        let conn = sqlite::open(&path).unwrap();

        configure_database_durability(&conn).unwrap();

        let mut journal = conn.prepare("PRAGMA journal_mode").unwrap();
        assert_eq!(journal.next().unwrap(), sqlite::State::Row);
        assert_eq!(journal.read::<String, _>(0).unwrap(), "wal");
        drop(journal);
        let mut synchronous = conn.prepare("PRAGMA synchronous").unwrap();
        assert_eq!(synchronous.next().unwrap(), sqlite::State::Row);
        assert_eq!(synchronous.read::<i64, _>(0).unwrap(), 2);
        drop(synchronous);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}
