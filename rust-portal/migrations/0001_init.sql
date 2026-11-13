CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    -- "argon2id$<salt hex>$<hash hex>" or, until first login, "guac-sha256$<salt hex>$<hash hex>"
    password_hash TEXT NOT NULL,
    totp_secret TEXT,
    totp_disabled BOOLEAN NOT NULL DEFAULT 0,
    totp_last_step INTEGER NOT NULL DEFAULT 0,
    is_admin BOOLEAN NOT NULL DEFAULT 0,
    disabled BOOLEAN NOT NULL DEFAULT 0
);

CREATE TABLE connections (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    hostname TEXT NOT NULL,
    port INTEGER NOT NULL DEFAULT 3389,
    username TEXT NOT NULL DEFAULT '',
    password TEXT NOT NULL DEFAULT '',
    domain TEXT
);

CREATE TABLE user_connections (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    connection_id INTEGER NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, connection_id)
);

CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
