use std::str::FromStr as _;

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};

pub async fn open(url: &str) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new().max_connections(8).connect_with(options).await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

#[derive(FromRow, Clone, Debug)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub totp_secret: Option<String>,
    pub totp_disabled: bool,
    pub totp_last_step: i64,
    pub is_admin: bool,
    pub disabled: bool,
    pub totp_pending: Option<String>,
}

#[derive(FromRow, Serialize, Clone, Debug)]
pub struct ConnectionSummary {
    pub id: i64,
    pub name: String,
}

#[derive(FromRow, Clone)]
pub struct Connection {
    pub id: i64,
    pub name: String,
    pub hostname: String,
    pub port: i64,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn user_by_name(pool: &SqlitePool, username: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as("SELECT * FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(pool)
        .await
}

pub async fn user_by_session(pool: &SqlitePool, token_hash: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as(
        "SELECT u.* FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = ? AND s.expires_at > ? AND NOT u.disabled",
    )
    .bind(token_hash)
    .bind(now_unix())
    .fetch_optional(pool)
    .await
}

pub async fn create_session(pool: &SqlitePool, token_hash: &str, user_id: i64, ttl_secs: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now_unix())
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(token_hash)
        .bind(user_id)
        .bind(now_unix() + ttl_secs)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_session(pool: &SqlitePool, token_hash: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_password_hash(pool: &SqlitePool, user_id: i64, hash: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records the TOTP time step just used, so the same code cannot be replayed.
/// Returns false when a code from this step (or a later one) was already accepted.
pub async fn claim_totp_step(pool: &SqlitePool, user_id: i64, step: i64) -> sqlx::Result<bool> {
    let done = sqlx::query("UPDATE users SET totp_last_step = ? WHERE id = ? AND totp_last_step < ?")
        .bind(step)
        .bind(user_id)
        .bind(step)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn connections_for(pool: &SqlitePool, user: &User) -> sqlx::Result<Vec<ConnectionSummary>> {
    if user.is_admin {
        sqlx::query_as("SELECT id, name FROM connections ORDER BY name").fetch_all(pool).await
    } else {
        sqlx::query_as(
            "SELECT c.id, c.name FROM connections c JOIN user_connections uc ON uc.connection_id = c.id
             WHERE uc.user_id = ? ORDER BY c.name",
        )
        .bind(user.id)
        .fetch_all(pool)
        .await
    }
}

pub async fn connection_for(pool: &SqlitePool, user: &User, id: i64) -> sqlx::Result<Option<Connection>> {
    sqlx::query_as(
        "SELECT c.* FROM connections c WHERE c.id = ? AND (? OR EXISTS (
            SELECT 1 FROM user_connections uc WHERE uc.connection_id = c.id AND uc.user_id = ?))",
    )
    .bind(id)
    .bind(user.is_admin)
    .bind(user.id)
    .fetch_optional(pool)
    .await
}

pub async fn connection_by_id(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Connection>> {
    sqlx::query_as("SELECT * FROM connections WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

// ---------- two-factor enrollment ----------

pub async fn set_totp_pending(pool: &SqlitePool, user_id: i64, secret: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE users SET totp_pending = ? WHERE id = ?")
        .bind(secret)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Makes the pending secret the user's secret, recording the step just used.
pub async fn confirm_totp(pool: &SqlitePool, user_id: i64, step: i64) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE users SET totp_secret = totp_pending, totp_pending = NULL, totp_last_step = ?
         WHERE id = ? AND totp_pending IS NOT NULL AND totp_secret IS NULL",
    )
    .bind(step)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

// ---------- administration ----------

#[derive(FromRow, Serialize)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub is_admin: bool,
    pub disabled: bool,
    pub totp_disabled: bool,
    pub totp_enrolled: bool,
    /// Comma-separated connection ids (SQLite has no arrays).
    #[serde(skip)]
    pub connection_ids: Option<String>,
}

pub async fn list_users(pool: &SqlitePool) -> sqlx::Result<Vec<UserRow>> {
    sqlx::query_as(
        "SELECT u.id, u.username, u.is_admin, u.disabled, u.totp_disabled,
                u.totp_secret IS NOT NULL AS totp_enrolled,
                (SELECT group_concat(connection_id) FROM user_connections WHERE user_id = u.id) AS connection_ids
         FROM users u ORDER BY u.username COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await
}

pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
    is_admin: bool,
    totp_disabled: bool,
) -> sqlx::Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO users (username, password_hash, is_admin, totp_disabled) VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(username)
    .bind(password_hash)
    .bind(is_admin)
    .bind(totp_disabled)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn user_by_id(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<User>> {
    sqlx::query_as("SELECT * FROM users WHERE id = ?").bind(id).fetch_optional(pool).await
}

/// Sets one boolean column; `column` comes from a fixed list, never from input.
pub async fn set_user_flag(pool: &SqlitePool, id: i64, column: UserFlag, value: bool) -> sqlx::Result<()> {
    let sql = match column {
        UserFlag::Admin => "UPDATE users SET is_admin = ? WHERE id = ?",
        UserFlag::Disabled => "UPDATE users SET disabled = ? WHERE id = ?",
        UserFlag::TotpDisabled => "UPDATE users SET totp_disabled = ? WHERE id = ?",
    };
    sqlx::query(sql).bind(value).bind(id).execute(pool).await?;
    Ok(())
}

#[derive(Clone, Copy)]
pub enum UserFlag {
    Admin,
    Disabled,
    TotpDisabled,
}

/// Forgets the user's authenticator; they set up a new one at next sign-in.
pub async fn reset_totp(pool: &SqlitePool, id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE users SET totp_secret = NULL, totp_pending = NULL, totp_last_step = 0 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Length of one traffic bucket, seconds.
const TRAFFIC_BUCKET: i64 = 600;
/// How long traffic rows are kept, seconds.
const TRAFFIC_KEEP: i64 = 8 * 24 * 3600;

/// Adds media traffic of one session to the current bucket.
pub async fn add_traffic(
    pool: &SqlitePool,
    user_id: i64,
    connection_id: i64,
    bytes_out: u64,
    bytes_in: u64,
    seconds: i64,
) -> sqlx::Result<()> {
    let now = now_unix();
    let bucket = now - now.rem_euclid(TRAFFIC_BUCKET);
    sqlx::query(
        "INSERT INTO traffic (user_id, connection_id, bucket, bytes_out, bytes_in, seconds) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (user_id, connection_id, bucket) DO UPDATE SET
             bytes_out = bytes_out + excluded.bytes_out,
             bytes_in = bytes_in + excluded.bytes_in,
             seconds = seconds + excluded.seconds",
    )
    .bind(user_id)
    .bind(connection_id)
    .bind(bucket)
    .bind(i64::try_from(bytes_out).unwrap_or(i64::MAX))
    .bind(i64::try_from(bytes_in).unwrap_or(i64::MAX))
    .bind(seconds)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM traffic WHERE bucket < ?").bind(now - TRAFFIC_KEEP).execute(pool).await?;
    Ok(())
}

#[derive(Serialize, FromRow)]
pub struct TrafficRow {
    pub username: String,
    pub connection: Option<String>,
    pub bytes_out: i64,
    pub bytes_in: i64,
    pub seconds: i64,
}

/// Traffic per user and computer since `since` (unix time), busiest first.
pub async fn traffic_since(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<TrafficRow>> {
    sqlx::query_as(
        "SELECT u.username, c.name AS connection,
                SUM(t.bytes_out) AS bytes_out, SUM(t.bytes_in) AS bytes_in, SUM(t.seconds) AS seconds
         FROM traffic t JOIN users u ON u.id = t.user_id LEFT JOIN connections c ON c.id = t.connection_id
         WHERE t.bucket >= ?
         GROUP BY t.user_id, t.connection_id
         ORDER BY SUM(t.bytes_out) + SUM(t.bytes_in) DESC",
    )
    .bind(since - since.rem_euclid(TRAFFIC_BUCKET))
    .fetch_all(pool)
    .await
}

pub async fn delete_user(pool: &SqlitePool, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM users WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

/// Signs the user out everywhere.
pub async fn delete_user_sessions(pool: &SqlitePool, user_id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = ?").bind(user_id).execute(pool).await?;
    Ok(())
}

/// Replaces the computers a user may open.
pub async fn set_user_connections(pool: &SqlitePool, user_id: i64, ids: &[i64]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM user_connections WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for id in ids {
        sqlx::query("INSERT OR IGNORE INTO user_connections (user_id, connection_id) SELECT ?, id FROM connections WHERE id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

/// A computer as the admin page shows it: everything except the password.
#[derive(FromRow, Serialize)]
pub struct ConnectionRow {
    pub id: i64,
    pub name: String,
    pub hostname: String,
    pub port: i64,
    pub username: String,
    pub domain: Option<String>,
    pub has_password: bool,
}

pub async fn list_connections(pool: &SqlitePool) -> sqlx::Result<Vec<ConnectionRow>> {
    sqlx::query_as(
        "SELECT id, name, hostname, port, username, domain, password != '' AS has_password
         FROM connections ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await
}

pub struct ConnectionInput<'a> {
    pub name: &'a str,
    pub hostname: &'a str,
    pub port: i64,
    pub username: &'a str,
    pub domain: Option<&'a str>,
    /// None keeps the stored password (on update).
    pub password: Option<&'a str>,
}

pub async fn create_connection(pool: &SqlitePool, c: &ConnectionInput<'_>) -> sqlx::Result<i64> {
    let row: (i64,) = sqlx::query_as(
        // Ids from 100000 up, so `rust-portal import` (which keeps Guacamole's
        // connection ids) never overwrites a computer added here.
        "INSERT INTO connections (id, name, hostname, port, username, password, domain)
         VALUES ((SELECT max(coalesce(max(id), 0) + 1, 100000) FROM connections), ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(c.name)
    .bind(c.hostname)
    .bind(c.port)
    .bind(c.username)
    .bind(c.password.unwrap_or(""))
    .bind(c.domain)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn update_connection(pool: &SqlitePool, id: i64, c: &ConnectionInput<'_>) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE connections SET name = ?, hostname = ?, port = ?, username = ?, domain = ?,
                password = COALESCE(?, password)
         WHERE id = ?",
    )
    .bind(c.name)
    .bind(c.hostname)
    .bind(c.port)
    .bind(c.username)
    .bind(c.domain)
    .bind(c.password)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn delete_connection(pool: &SqlitePool, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM connections WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}
