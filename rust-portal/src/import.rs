//! One-way import of users, TOTP keys, RDP connections and permissions from
//! the Guacamole Postgres database. Safe to re-run: rows are upserted by
//! username / Guacamole connection id, and our own password hashes (already
//! upgraded to Argon2) and TOTP replay state are kept.

use std::collections::HashMap;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Row as _, SqlitePool};
use tracing::info;

pub async fn run(pool: &SqlitePool, guac_url: &str) -> anyhow::Result<()> {
    let pg = PgPoolOptions::new().max_connections(1).connect(guac_url).await?;

    // Users, with admin flag and TOTP attributes.
    let users = sqlx::query(
        "SELECT u.user_id, e.name, u.password_hash, u.password_salt, u.disabled,
                EXISTS (SELECT 1 FROM guacamole_system_permission p
                        WHERE p.entity_id = u.entity_id AND p.permission = 'ADMINISTER') AS is_admin,
                (SELECT attribute_value FROM guacamole_user_attribute a
                  WHERE a.user_id = u.user_id AND a.attribute_name = 'guac-totp-key-secret') AS totp_secret,
                (SELECT attribute_value FROM guacamole_user_attribute a
                  WHERE a.user_id = u.user_id AND a.attribute_name = 'guac-totp-key-confirmed') AS totp_confirmed,
                (SELECT attribute_value FROM guacamole_user_attribute a
                  WHERE a.user_id = u.user_id AND a.attribute_name = 'guac-totp-disabled') AS totp_disabled
         FROM guacamole_user u JOIN guacamole_entity e ON e.entity_id = u.entity_id",
    )
    .fetch_all(&pg)
    .await?;

    let mut tx = pool.begin().await?;
    let mut user_ids: HashMap<String, i64> = HashMap::new();
    for row in &users {
        let name: String = row.get("name");
        let hash: Vec<u8> = row.get("password_hash");
        let salt: Option<Vec<u8>> = row.get("password_salt");
        let disabled: bool = row.get("disabled");
        let is_admin: bool = row.get("is_admin");
        let confirmed = row.get::<Option<String>, _>("totp_confirmed").as_deref() == Some("true");
        let totp_secret: Option<String> = if confirmed { row.get("totp_secret") } else { None };
        let totp_disabled = row.get::<Option<String>, _>("totp_disabled").as_deref() == Some("true");
        let stored = format!(
            "guac-sha256${}${}",
            salt.map(hex::encode_upper).unwrap_or_default(),
            hex::encode(hash)
        );

        // Keep an Argon2 hash we already made, unless the Guacamole password changed since:
        // we cannot tell, so the Guacamole hash wins only for users still on it.
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO users (username, password_hash, totp_secret, totp_disabled, is_admin, disabled)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (username) DO UPDATE SET
                password_hash = CASE WHEN users.password_hash LIKE 'argon2id$%' THEN users.password_hash
                                     ELSE excluded.password_hash END,
                -- Keep an authenticator set up on this portal if Guacamole has none.
                totp_secret = COALESCE(excluded.totp_secret, users.totp_secret),
                totp_disabled = excluded.totp_disabled,
                is_admin = excluded.is_admin,
                disabled = excluded.disabled
             RETURNING id",
        )
        .bind(&name)
        .bind(&stored)
        .bind(&totp_secret)
        .bind(totp_disabled)
        .bind(is_admin)
        .bind(disabled)
        .fetch_one(&mut *tx)
        .await?;
        user_ids.insert(name, id);
    }

    // RDP connections and their parameters.
    let conns = sqlx::query("SELECT connection_id, connection_name FROM guacamole_connection WHERE protocol = 'rdp'")
        .fetch_all(&pg)
        .await?;
    let params = sqlx::query(
        "SELECT connection_id, parameter_name, parameter_value FROM guacamole_connection_parameter
         WHERE parameter_name IN ('hostname', 'port', 'username', 'password', 'domain')",
    )
    .fetch_all(&pg)
    .await?;
    let mut by_conn: HashMap<i32, HashMap<String, String>> = HashMap::new();
    for p in &params {
        by_conn
            .entry(p.get("connection_id"))
            .or_default()
            .insert(p.get("parameter_name"), p.get("parameter_value"));
    }
    for c in &conns {
        let id: i32 = c.get("connection_id");
        let name: String = c.get("connection_name");
        let p = by_conn.remove(&id).unwrap_or_default();
        let get = |k: &str| p.get(k).cloned().unwrap_or_default();
        sqlx::query(
            "INSERT INTO connections (id, name, hostname, port, username, password, domain)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, hostname = excluded.hostname,
                port = excluded.port, username = excluded.username, password = excluded.password,
                domain = excluded.domain",
        )
        .bind(i64::from(id))
        .bind(&name)
        .bind(get("hostname"))
        .bind(get("port").parse::<i64>().unwrap_or(3389))
        .bind(get("username"))
        .bind(get("password"))
        .bind(p.get("domain").filter(|d| !d.is_empty()))
        .execute(&mut *tx)
        .await?;
    }

    // Direct user -> connection READ permissions (replaced wholesale).
    let perms = sqlx::query(
        "SELECT e.name, cp.connection_id FROM guacamole_connection_permission cp
         JOIN guacamole_entity e ON e.entity_id = cp.entity_id
         WHERE cp.permission = 'READ' AND e.type = 'USER'",
    )
    .fetch_all(&pg)
    .await?;
    sqlx::query("DELETE FROM user_connections").execute(&mut *tx).await?;
    let mut granted = 0;
    for p in &perms {
        let name: String = p.get("name");
        let conn: i32 = p.get("connection_id");
        if let Some(uid) = user_ids.get(&name) {
            granted += sqlx::query(
                "INSERT OR IGNORE INTO user_connections (user_id, connection_id)
                 SELECT ?, id FROM connections WHERE id = ?",
            )
            .bind(uid)
            .bind(i64::from(conn))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
    }
    tx.commit().await?;

    info!(
        users = users.len(),
        connections = conns.len(),
        permissions = granted,
        "import finished"
    );
    println!(
        "imported {} users, {} connections, {} permissions",
        users.len(),
        conns.len(),
        granted
    );
    Ok(())
}
