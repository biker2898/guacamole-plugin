//! HTTP API, static client and the session WebSocket.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::auth::{self, LoginBan};
use crate::db::{self, ConnectionInput, User, UserFlag};
use crate::Config;

const COOKIE: &str = "rp_session";
const SESSION_TTL_SECS: i64 = 12 * 3600;

pub(crate) struct App {
    pool: SqlitePool,
    config: Arc<Config>,
    ban: LoginBan,
    /// Viewer sessions in progress, for the admin page.
    live: Mutex<HashMap<u64, LiveSession>>,
    next_live_id: AtomicU64,
}

#[derive(Serialize, Clone)]
struct LiveSession {
    id: u64,
    user_id: i64,
    username: String,
    connection_id: i64,
    connection: String,
    started: i64,
    #[serde(skip)]
    stop: Arc<Notify>,
}

impl App {
    /// Ends every viewer session of `user_id`, e.g. after disabling the account.
    fn end_sessions_of(&self, user_id: i64) {
        for s in self.live.lock().unwrap().values().filter(|s| s.user_id == user_id) {
            s.stop.notify_one();
        }
    }
}

/// Removes a viewer session from the registry when it ends, however it ends.
struct LiveGuard(Arc<App>, u64);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.live.lock().unwrap().remove(&self.1);
    }
}

type AppState = State<Arc<App>>;

pub async fn serve(config: Config, pool: SqlitePool) -> anyhow::Result<()> {
    let listen = config.listen;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(%listen, "listening");
    axum::serve(listener, router(config, pool).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

pub(crate) fn router(config: Config, pool: SqlitePool) -> Router {
    let app = Arc::new(App {
        pool,
        config: Arc::new(config),
        ban: LoginBan::new(50, Duration::from_secs(900)),
        live: Mutex::new(HashMap::new()),
        next_live_id: AtomicU64::new(1),
    });
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/connections", get(connections))
        .route("/api/session", get(session_ws))
        .route("/admin.js", get(admin_js))
        .route("/api/admin/users", get(admin_users).post(admin_create_user))
        .route("/api/admin/users/{id}", post(admin_update_user).delete(admin_delete_user))
        .route("/api/admin/connections", get(admin_connections).post(admin_create_connection))
        .route(
            "/api/admin/connections/{id}",
            post(admin_update_connection).delete(admin_delete_connection),
        )
        .route("/api/admin/sessions", get(admin_sessions))
        .route("/api/admin/sessions/{id}/end", post(admin_end_session))
        .with_state(app)
}

const APP_JS: &str = include_str!("../web/app.js");
const ADMIN_JS: &str = include_str!("../web/admin.js");

/// The page, with each script URL carrying a hash of its contents, so caches
/// in between (Cloudflare keeps .js for hours) never serve an old script.
static INDEX: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let version = |text: &str| {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        format!("{:x}", h.finish())
    };
    include_str!("../web/index.html")
        .replace(r#"src="app.js""#, &format!(r#"src="app.js?v={}""#, version(APP_JS)))
        .replace(r#"src="admin.js""#, &format!(r#"src="admin.js?v={}""#, version(ADMIN_JS)))
});

/// Browsers and proxies must check back before reusing these.
const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-cache");

async fn index() -> impl IntoResponse {
    ([NO_CACHE], Html(INDEX.as_str()))
}

async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), NO_CACHE], APP_JS)
}

async fn admin_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), NO_CACHE], ADMIN_JS)
}

/// nginx sets X-Real-IP; we only listen on localhost, so it can be trusted.
/// Account name shown in authenticator apps.
const TOTP_ISSUER: &str = "2FA SW";

fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(peer.ip())
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_owned())
}

async fn current_user(app: &App, headers: &HeaderMap) -> Option<User> {
    let token = session_token(headers)?;
    db::user_by_session(&app.pool, &auth::token_hash(&token)).await.ok().flatten()
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
    #[serde(default)]
    otp: String,
}

async fn login(
    State(app): AppState,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    let ip = client_ip(&headers, peer);
    if app.ban.is_banned(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "Too many failed logins. Try again in 15 minutes.");
    }

    let user = db::user_by_name(&app.pool, req.username.trim()).await.ok().flatten();
    let stored = user
        .as_ref()
        .filter(|u| !u.disabled)
        .map(|u| u.password_hash.clone())
        // Hash anyway for unknown users so timing does not reveal which names exist.
        .unwrap_or_else(|| "argon2id$00000000000000000000000000000000$00".to_owned());
    let password = req.password.clone();
    let ok = tokio::task::spawn_blocking(move || auth::verify_password(&stored, &password))
        .await
        .unwrap_or(false);
    let Some(user) = user.filter(|u| ok && !u.disabled) else {
        app.ban.record_failure(ip);
        warn!(%ip, username = %req.username, "login failed");
        return error(StatusCode::UNAUTHORIZED, "Invalid username or password.");
    };

    if !user.totp_disabled && user.totp_secret.is_none() {
        // First sign-in without an authenticator: show a new secret, and accept
        // it once the user types a code from it.
        let pending = match &user.totp_pending {
            Some(p) => p.clone(),
            None => {
                let secret = auth::new_totp_secret();
                if let Err(e) = db::set_totp_pending(&app.pool, user.id, &secret).await {
                    warn!("set totp_pending: {e}");
                    return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not start two-factor setup.");
                }
                secret
            }
        };
        let setup = || {
            // Authenticator apps show this name with the username, not the site's domain.
            let (uri, qr) = auth::totp_enrollment(TOTP_ISSUER, &user.username, &pending);
            json!({ "secret": pending, "uri": uri, "qr": qr })
        };
        if req.otp.trim().is_empty() {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "otp_setup": setup() }))).into_response();
        }
        let confirmed = match auth::verify_totp(&pending, &req.otp) {
            Some(step) => db::confirm_totp(&app.pool, user.id, step).await.unwrap_or(false),
            None => false,
        };
        if !confirmed {
            app.ban.record_failure(ip);
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "otp_setup": setup(), "error": "That code does not match. Check the time on your phone." })),
            )
                .into_response();
        }
        info!(%ip, username = %user.username, "two-factor authentication set up");
    } else if !user.totp_disabled {
        let secret = user.totp_secret.as_deref().unwrap_or_default();
        if req.otp.trim().is_empty() {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "otp_required": true }))).into_response();
        }
        let accepted = match auth::verify_totp(secret, &req.otp) {
            Some(step) => db::claim_totp_step(&app.pool, user.id, step).await.unwrap_or(false),
            None => false,
        };
        if !accepted {
            app.ban.record_failure(ip);
            warn!(%ip, username = %user.username, "TOTP failed");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "otp_required": true, "error": "Invalid or already used code." })),
            )
                .into_response();
        }
    }

    if auth::needs_rehash(&user.password_hash) {
        let password = req.password.clone();
        if let Ok(Ok(hash)) = tokio::task::spawn_blocking(move || auth::hash_password(&password)).await {
            let _ = db::set_password_hash(&app.pool, user.id, &hash).await;
        }
    }

    let (token, hash) = auth::new_session_token();
    if let Err(e) = db::create_session(&app.pool, &hash, user.id, SESSION_TTL_SECS).await {
        warn!("create session: {e}");
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not start a session.");
    }
    app.ban.clear(ip);
    info!(%ip, username = %user.username, "login");
    let cookie = format!("{COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}");
    let mut response = Json(json!({ "username": user.username, "is_admin": user.is_admin })).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn logout(State(app): AppState, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        let _ = db::delete_session(&app.pool, &auth::token_hash(&token)).await;
    }
    let mut response = Json(json!({})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("rp_session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0"),
    );
    response
}

async fn me(State(app): AppState, headers: HeaderMap) -> Response {
    match current_user(&app, &headers).await {
        Some(u) => Json(json!({ "username": u.username, "is_admin": u.is_admin })).into_response(),
        None => error(StatusCode::UNAUTHORIZED, "Not signed in."),
    }
}

async fn connections(State(app): AppState, headers: HeaderMap) -> Response {
    let Some(user) = current_user(&app, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Not signed in.");
    };
    match db::connections_for(&app.pool, &user).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            warn!("list connections: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "Could not list computers.")
        }
    }
}

#[derive(Deserialize)]
struct SessionQuery {
    id: i64,
}

/// Same-origin check for the WebSocket (browsers do not apply CORS to it).
fn same_origin(headers: &HeaderMap) -> bool {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    matches!((origin, host), (Some(o), Some(h)) if o == format!("https://{h}"))
}

async fn session_ws(
    State(app): AppState,
    headers: HeaderMap,
    Query(q): Query<SessionQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return error(StatusCode::FORBIDDEN, "Bad origin.");
    }
    let Some(user) = current_user(&app, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Not signed in.");
    };
    let conn = match db::connection_for(&app.pool, &user, q.id).await {
        Ok(Some(c)) => c,
        _ => return error(StatusCode::NOT_FOUND, "No such computer."),
    };
    let config = app.config.clone();
    ws.on_upgrade(move |socket| async move {
        let id = app.next_live_id.fetch_add(1, Ordering::Relaxed);
        let stop = Arc::new(Notify::new());
        app.live.lock().unwrap().insert(
            id,
            LiveSession {
                id,
                user_id: user.id,
                username: user.username.clone(),
                connection_id: conn.id,
                connection: conn.name.clone(),
                started: db::now_unix(),
                stop: stop.clone(),
            },
        );
        let _guard = LiveGuard(app.clone(), id);
        crate::session::run(socket, conn, config, user.username, stop).await;
    })
}

// ---------- administration ----------

/// The signed-in admin, or the response to send instead. Changes also need a
/// same-origin request (the cookie is SameSite=Strict too).
async fn require_admin(app: &App, headers: &HeaderMap, change: bool) -> Result<User, Response> {
    if change && !same_origin(headers) {
        return Err(error(StatusCode::FORBIDDEN, "Bad origin."));
    }
    match current_user(app, headers).await {
        Some(u) if u.is_admin => Ok(u),
        Some(_) => Err(error(StatusCode::FORBIDDEN, "Administrators only.")),
        None => Err(error(StatusCode::UNAUTHORIZED, "Not signed in.")),
    }
}

fn db_error(what: &str, e: sqlx::Error) -> Response {
    if let sqlx::Error::Database(d) = &e {
        if d.is_unique_violation() {
            return error(StatusCode::CONFLICT, "That name is already taken.");
        }
    }
    warn!("{what}: {e}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "Database error.")
}

fn ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

async fn admin_users(State(app): AppState, headers: HeaderMap) -> Response {
    if let Err(r) = require_admin(&app, &headers, false).await {
        return r;
    }
    match db::list_users(&app.pool).await {
        Ok(rows) => {
            let list: Vec<_> = rows
                .into_iter()
                .map(|u| {
                    let connections: Vec<i64> = u
                        .connection_ids
                        .as_deref()
                        .unwrap_or("")
                        .split(',')
                        .filter_map(|s| s.parse().ok())
                        .collect();
                    let mut v = serde_json::to_value(&u).unwrap_or_default();
                    v["connections"] = json!(connections);
                    v
                })
                .collect();
            Json(list).into_response()
        }
        Err(e) => db_error("list users", e),
    }
}

fn valid_username(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

const MIN_PASSWORD: usize = 8;

async fn hash(password: String) -> Result<String, Response> {
    match tokio::task::spawn_blocking(move || auth::hash_password(&password)).await {
        Ok(Ok(h)) => Ok(h),
        _ => Err(error(StatusCode::INTERNAL_SERVER_ERROR, "Could not hash the password.")),
    }
}

#[derive(Deserialize)]
struct NewUser {
    username: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
    #[serde(default)]
    totp_disabled: bool,
    #[serde(default)]
    connections: Vec<i64>,
}

async fn admin_create_user(State(app): AppState, headers: HeaderMap, Json(req): Json<NewUser>) -> Response {
    let admin = match require_admin(&app, &headers, true).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let username = req.username.trim();
    if !valid_username(username) {
        return error(StatusCode::BAD_REQUEST, "Username must be 1-64 characters, no spaces.");
    }
    if req.password.chars().count() < MIN_PASSWORD {
        return error(StatusCode::BAD_REQUEST, "Password must be at least 8 characters.");
    }
    let hash = match hash(req.password).await {
        Ok(h) => h,
        Err(r) => return r,
    };
    let id = match db::create_user(&app.pool, username, &hash, req.is_admin, req.totp_disabled).await {
        Ok(id) => id,
        Err(e) => return db_error("create user", e),
    };
    if let Err(e) = db::set_user_connections(&app.pool, id, &req.connections).await {
        return db_error("set user connections", e);
    }
    info!(admin = %admin.username, user = %username, "user created");
    Json(json!({ "ok": true, "id": id })).into_response()
}

#[derive(Deserialize)]
struct UserUpdate {
    is_admin: Option<bool>,
    disabled: Option<bool>,
    totp_disabled: Option<bool>,
    password: Option<String>,
    #[serde(default)]
    reset_totp: bool,
    connections: Option<Vec<i64>>,
}

async fn admin_update_user(
    State(app): AppState,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UserUpdate>,
) -> Response {
    let admin = match require_admin(&app, &headers, true).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let Ok(Some(target)) = db::user_by_id(&app.pool, id).await else {
        return error(StatusCode::NOT_FOUND, "No such user.");
    };
    // Keep at least the acting admin able to get back in.
    if id == admin.id && (req.is_admin == Some(false) || req.disabled == Some(true)) {
        return error(StatusCode::BAD_REQUEST, "You cannot remove your own admin rights or disable yourself.");
    }
    let hash = match &req.password {
        Some(p) if p.chars().count() < MIN_PASSWORD => {
            return error(StatusCode::BAD_REQUEST, "Password must be at least 8 characters.");
        }
        Some(p) => match hash(p.clone()).await {
            Ok(h) => Some(h),
            Err(r) => return r,
        },
        None => None,
    };

    let result: sqlx::Result<()> = async {
        for (flag, value) in [
            (UserFlag::Admin, req.is_admin),
            (UserFlag::Disabled, req.disabled),
            (UserFlag::TotpDisabled, req.totp_disabled),
        ] {
            if let Some(v) = value {
                db::set_user_flag(&app.pool, id, flag, v).await?;
            }
        }
        if let Some(h) = &hash {
            db::set_password_hash(&app.pool, id, h).await?;
        }
        if req.reset_totp {
            db::reset_totp(&app.pool, id).await?;
        }
        if let Some(ids) = &req.connections {
            db::set_user_connections(&app.pool, id, ids).await?;
        }
        // New password or disabled account: sign them out everywhere (except
        // the admin changing their own password in this browser).
        if req.disabled == Some(true) || (hash.is_some() && id != admin.id) {
            db::delete_user_sessions(&app.pool, id).await?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        return db_error("update user", e);
    }
    if req.disabled == Some(true) {
        app.end_sessions_of(id);
    }
    info!(
        admin = %admin.username,
        user = %target.username,
        is_admin = ?req.is_admin,
        disabled = ?req.disabled,
        totp_disabled = ?req.totp_disabled,
        password = hash.is_some(),
        reset_totp = req.reset_totp,
        connections = ?req.connections,
        "user updated"
    );
    ok()
}

async fn admin_delete_user(State(app): AppState, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let admin = match require_admin(&app, &headers, true).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    if id == admin.id {
        return error(StatusCode::BAD_REQUEST, "You cannot delete yourself.");
    }
    let Ok(Some(target)) = db::user_by_id(&app.pool, id).await else {
        return error(StatusCode::NOT_FOUND, "No such user.");
    };
    if let Err(e) = db::delete_user(&app.pool, id).await {
        return db_error("delete user", e);
    }
    app.end_sessions_of(id);
    info!(admin = %admin.username, user = %target.username, "user deleted");
    ok()
}

async fn admin_connections(State(app): AppState, headers: HeaderMap) -> Response {
    if let Err(r) = require_admin(&app, &headers, false).await {
        return r;
    }
    match db::list_connections(&app.pool).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => db_error("list connections", e),
    }
}

#[derive(Deserialize)]
struct ConnectionForm {
    name: String,
    hostname: String,
    #[serde(default = "default_port")]
    port: i64,
    #[serde(default)]
    username: String,
    #[serde(default)]
    domain: String,
    /// Empty or missing keeps the stored password when editing.
    #[serde(default)]
    password: String,
}

fn default_port() -> i64 {
    3389
}

impl ConnectionForm {
    fn validate(&self) -> Result<ConnectionInput<'_>, Response> {
        let name = self.name.trim();
        let hostname = self.hostname.trim();
        if name.is_empty() || name.len() > 128 {
            return Err(error(StatusCode::BAD_REQUEST, "Name must be 1-128 characters."));
        }
        if hostname.is_empty() || hostname.len() > 253 || hostname.chars().any(|c| c.is_whitespace()) {
            return Err(error(StatusCode::BAD_REQUEST, "Enter a hostname or IP address."));
        }
        if !(1..=65535).contains(&self.port) {
            return Err(error(StatusCode::BAD_REQUEST, "Port must be 1-65535."));
        }
        let domain = self.domain.trim();
        Ok(ConnectionInput {
            name,
            hostname,
            port: self.port,
            username: self.username.trim(),
            domain: (!domain.is_empty()).then_some(domain),
            password: (!self.password.is_empty()).then_some(self.password.as_str()),
        })
    }
}

async fn admin_create_connection(
    State(app): AppState,
    headers: HeaderMap,
    Json(form): Json<ConnectionForm>,
) -> Response {
    let admin = match require_admin(&app, &headers, true).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let input = match form.validate() {
        Ok(i) => i,
        Err(r) => return r,
    };
    match db::create_connection(&app.pool, &input).await {
        Ok(id) => {
            info!(admin = %admin.username, connection = %input.name, "computer added");
            Json(json!({ "ok": true, "id": id })).into_response()
        }
        Err(e) => db_error("create connection", e),
    }
}

async fn admin_update_connection(
    State(app): AppState,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(form): Json<ConnectionForm>,
) -> Response {
    let admin = match require_admin(&app, &headers, true).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let input = match form.validate() {
        Ok(i) => i,
        Err(r) => return r,
    };
    match db::update_connection(&app.pool, id, &input).await {
        Ok(true) => {
            info!(admin = %admin.username, connection = %input.name, password = input.password.is_some(), "computer updated");
            ok()
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "No such computer."),
        Err(e) => db_error("update connection", e),
    }
}

async fn admin_delete_connection(State(app): AppState, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let admin = match require_admin(&app, &headers, true).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    match db::delete_connection(&app.pool, id).await {
        Ok(()) => {
            info!(admin = %admin.username, connection = id, "computer deleted");
            ok()
        }
        Err(e) => db_error("delete connection", e),
    }
}

async fn admin_sessions(State(app): AppState, headers: HeaderMap) -> Response {
    if let Err(r) = require_admin(&app, &headers, false).await {
        return r;
    }
    let mut list: Vec<LiveSession> = app.live.lock().unwrap().values().cloned().collect();
    list.sort_by_key(|s| s.started);
    Json(list).into_response()
}

async fn admin_end_session(State(app): AppState, headers: HeaderMap, Path(id): Path<u64>) -> Response {
    let admin = match require_admin(&app, &headers, true).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let session = app.live.lock().unwrap().get(&id).cloned();
    match session {
        Some(s) => {
            s.stop.notify_one();
            info!(admin = %admin.username, user = %s.username, connection = %s.connection, "session ended by admin");
            ok()
        }
        None => error(StatusCode::NOT_FOUND, "That session already ended."),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use super::*;

    const HOST: &str = "portal.test";

    struct T {
        router: Router,
        pool: SqlitePool,
        _dir: TempDb,
    }

    struct TempDb(std::path::PathBuf);
    impl Drop for TempDb {
        fn drop(&mut self) {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
            }
        }
    }

    async fn setup() -> T {
        let path = std::env::temp_dir().join(format!("rp-test-{}.db", auth::new_session_token().0));
        let pool = db::open(&format!("sqlite://{}", path.display())).await.unwrap();
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            public_ip: "127.0.0.1".parse().unwrap(),
            udp_ports: (50000, 50001),
            db_url: String::new(),
            max_bitrate: 1_000_000,
        };
        let router = router(config, pool.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))));
        T { router, pool, _dir: TempDb(path) }
    }

    async fn add_user(pool: &SqlitePool, name: &str, pw: &str, admin: bool, totp: Option<&str>) -> i64 {
        let id = db::create_user(pool, name, &auth::hash_password(pw).unwrap(), admin, totp.is_none())
            .await
            .unwrap();
        if let Some(secret) = totp {
            sqlx::query("UPDATE users SET totp_secret = ? WHERE id = ?").bind(secret).bind(id).execute(pool).await.unwrap();
        }
        id
    }

    fn code(secret: &str) -> String {
        totp_rs::Builder::new()
            .with_secret(totp_rs::Secret::try_from_base32(secret).unwrap())
            .build_noncompliant()
            .generate_current()
            .to_string()
    }

    async fn call(t: &T, method: &str, path: &str, cookie: Option<&str>, origin: bool, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value, Option<String>) {
        let mut req = Request::builder().method(method).uri(path).header(header::HOST, HOST);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        if origin {
            req = req.header(header::ORIGIN, format!("https://{HOST}"));
        }
        let req = match body {
            Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = t.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(str::to_owned);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default(), cookie)
    }

    async fn login(t: &T, user: &str, pw: &str, otp: &str) -> (StatusCode, serde_json::Value, Option<String>) {
        call(t, "POST", "/api/login", None, true, Some(json!({ "username": user, "password": pw, "otp": otp }))).await
    }

    #[tokio::test]
    async fn admin_console() {
        let t = setup().await;
        let flame_secret = auth::new_totp_secret();
        let biker = add_user(&t.pool, "biker", "biker-pass", true, None).await;
        add_user(&t.pool, "flame", "flame-pass", true, Some(&flame_secret)).await;
        add_user(&t.pool, "worker", "worker-pass", false, None).await;

        // Admin without 2FA.
        let (s, me, cookie) = login(&t, "biker", "biker-pass", "").await;
        assert_eq!(s, StatusCode::OK, "{me}");
        assert_eq!(me["is_admin"], true);
        let biker_cookie = cookie.unwrap();

        // Admin with 2FA: code required, then accepted; the same code is not reusable.
        let (s, body, _) = login(&t, "flame", "flame-pass", "").await;
        assert_eq!((s, body["otp_required"].clone()), (StatusCode::UNAUTHORIZED, json!(true)));
        let c = code(&flame_secret);
        let (s, me, cookie) = login(&t, "flame", "flame-pass", &c).await;
        assert_eq!(s, StatusCode::OK, "{me}");
        assert_eq!(me["is_admin"], true);
        let flame_cookie = cookie.unwrap();
        let (s, _, _) = login(&t, "flame", "flame-pass", &c).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);

        // Both admins see the console; a plain user does not.
        for ck in [&biker_cookie, &flame_cookie] {
            let (s, users, _) = call(&t, "GET", "/api/admin/users", Some(ck), false, None).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(users.as_array().unwrap().len(), 3);
        }
        let (_, _, worker_cookie) = login(&t, "worker", "worker-pass", "").await;
        let worker_cookie = worker_cookie.unwrap();
        let (s, _, _) = call(&t, "GET", "/api/admin/users", Some(&worker_cookie), false, None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = call(&t, "GET", "/api/admin/users", None, false, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);

        // Computers: create, edit (password kept when empty), list.
        let new = json!({ "name": "VM-X", "hostname": "10.0.0.5", "port": 3389, "username": "u", "password": "p" });
        let (s, _, _) = call(&t, "POST", "/api/admin/connections", Some(&biker_cookie), false, Some(new.clone())).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "changes need a same-origin request");
        let (s, r, _) = call(&t, "POST", "/api/admin/connections", Some(&biker_cookie), true, Some(new)).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        let cid = r["id"].as_i64().unwrap();
        let edit = json!({ "name": "VM-Y", "hostname": "10.0.0.6", "port": 3390, "username": "u2", "password": "" });
        let (s, _, _) = call(&t, "POST", &format!("/api/admin/connections/{cid}"), Some(&flame_cookie), true, Some(edit)).await;
        assert_eq!(s, StatusCode::OK);
        let stored = db::connection_by_id(&t.pool, cid).await.unwrap().unwrap();
        assert_eq!((stored.name.as_str(), stored.port, stored.password.as_str()), ("VM-Y", 3390, "p"));

        // Users: create with a computer; it shows up in that user's list.
        let (s, r, _) = call(
            &t,
            "POST",
            "/api/admin/users",
            Some(&biker_cookie),
            true,
            Some(json!({ "username": "newbie", "password": "newbie-pass", "connections": [cid] })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{r}");
        let newbie = r["id"].as_i64().unwrap();
        let (s, body, _) = login(&t, "newbie", "newbie-pass", "").await;
        // 2FA is on by default: the first sign-in sets it up.
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let secret = body["otp_setup"]["secret"].as_str().unwrap().to_owned();
        assert!(body["otp_setup"]["qr"].as_str().unwrap().contains("<svg"));
        let (s, r, cookie) = login(&t, "newbie", "newbie-pass", &code(&secret)).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        let (_, list, _) = call(&t, "GET", "/api/connections", cookie.as_deref(), false, None).await;
        assert_eq!(list, json!([{ "id": cid, "name": "VM-Y" }]));
        let (_, users, _) = call(&t, "GET", "/api/admin/users", Some(&biker_cookie), false, None).await;
        let row = users.as_array().unwrap().iter().find(|u| u["username"] == "newbie").unwrap();
        assert_eq!((row["totp_enrolled"].clone(), row["connections"].clone()), (json!(true), json!([cid])));

        // Duplicate name, self-lockout, disable.
        let (s, _, _) = call(&t, "POST", "/api/admin/users", Some(&biker_cookie), true,
            Some(json!({ "username": "flame", "password": "whatever1" }))).await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, _) = call(&t, "POST", &format!("/api/admin/users/{biker}"), Some(&biker_cookie), true,
            Some(json!({ "is_admin": false }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = call(&t, "POST", &format!("/api/admin/users/{newbie}"), Some(&flame_cookie), true,
            Some(json!({ "disabled": true }))).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = call(&t, "GET", "/api/connections", cookie.as_deref(), false, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "disabling signs the user out");

        // Reset 2FA, change password, delete.
        let (s, _, _) = call(&t, "POST", &format!("/api/admin/users/{newbie}"), Some(&biker_cookie), true,
            Some(json!({ "disabled": false, "reset_totp": true, "password": "another-pass" }))).await;
        assert_eq!(s, StatusCode::OK);
        let (s, body, _) = login(&t, "newbie", "another-pass", "").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(body["otp_setup"]["secret"].as_str().unwrap() != secret);
        let (s, _, _) = call(&t, "DELETE", &format!("/api/admin/users/{newbie}"), Some(&biker_cookie), true, None).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = call(&t, "DELETE", &format!("/api/admin/connections/{cid}"), Some(&biker_cookie), true, None).await;
        assert_eq!(s, StatusCode::OK);
        let (_, sessions, _) = call(&t, "GET", "/api/admin/sessions", Some(&biker_cookie), false, None).await;
        assert_eq!(sessions, json!([]));
    }
}
