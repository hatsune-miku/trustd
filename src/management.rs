//! Human-facing management sessions and credential enrollment. Execution tokens
//! cannot authenticate here; WebAuthn challenge state never leaves the server.
use std::{collections::HashMap, time::Duration};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::HeaderMap,
    routing::{get, post},
};
use chrono::Utc;
use localshelld_sync_protocol::owner::{SignedStatement, Statement};
use serde::Deserialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::{
    auth::{self, AuthError, Credential},
    bridge,
    config::{load_or_create_seal_key, seal, seal_key_path},
    server::{ApiError, AppState, DaemonMode, strong_handle},
};

const SESSION_SECONDS: i64 = 600;
const CHALLENGE_SECONDS: i64 = 300;
const MAX_CHALLENGES: usize = 64;

enum Pending {
    Totp {
        owner: String,
        secret: Zeroizing<Vec<u8>>,
    },
}

/// A management login waiting for the owner's passkey on the backend.
struct PendingLogin {
    statement: String,
    backend_id: String,
    expires_at: i64,
}

#[derive(Default)]
pub struct Management {
    sessions: HashMap<String, i64>,
    challenges: HashMap<String, (i64, Pending)>,
    logins: HashMap<String, PendingLogin>,
}

impl Management {
    fn prune(&mut self) {
        let now = Utc::now().timestamp();
        self.sessions.retain(|_, expiry| *expiry > now);
        self.challenges.retain(|_, (expiry, _)| *expiry > now);
        self.logins.retain(|_, login| login.expires_at > now);
    }

    fn session(&mut self) -> Result<Value, ApiError> {
        self.prune();
        if self.sessions.len() >= 128 {
            return Err(ApiError::unavailable("too many management sessions"));
        }
        let session = strong_handle();
        let expires_at = Utc::now().timestamp() + SESSION_SECONDS;
        self.sessions.insert(session.clone(), expires_at);
        Ok(json!({"session":session, "expires_at":expires_at}))
    }

    fn insert(&mut self, pending: Pending) -> Result<String, ApiError> {
        self.prune();
        if self.challenges.len() >= MAX_CHALLENGES {
            return Err(ApiError::unavailable(
                "too many pending enrollments; retry later",
            ));
        }
        let id = strong_handle();
        self.challenges.insert(
            id.clone(),
            (Utc::now().timestamp() + CHALLENGE_SECONDS, pending),
        );
        Ok(id)
    }

    fn take(&mut self, id: &str) -> Result<Pending, ApiError> {
        self.prune();
        self.challenges
            .remove(id)
            .map(|(_, pending)| pending)
            .ok_or_else(|| ApiError::bad_request("challenge expired or already used; start again"))
    }
}

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| AuthError::InvalidCredential.into())
}

async fn owner(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    managed_by_user_daemon(state).await?;
    let bearer = bearer(headers)?;
    let mut management = state.management.lock().await;
    management.prune();
    if management.sessions.contains_key(bearer) {
        Ok(bearer.into())
    } else {
        Err(AuthError::InvalidCredential.into())
    }
}

pub async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    credential: Option<&Credential>,
) -> Result<(), ApiError> {
    if let Some(credential) = credential {
        return state.authenticate(credential).await;
    }
    owner(state, headers).await.map(|_| ())
}

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/admin/login", post(login))
        .route("/v1/admin/login/passkey/start", post(passkey_login_start))
        .route("/v1/admin/login/passkey/poll", post(passkey_login_poll))
        .route("/v1/admin/logout", post(logout))
        .route("/v1/admin/settings", get(settings))
        .route("/v1/admin/secrets", get(secrets))
        .route("/v1/admin/secrets/put", post(secret_put))
        .route("/v1/admin/secrets/remove", post(secret_remove))
        .route("/v1/admin/secrets/export", post(secret_export))
        .route(
            "/v1/admin/secrets/import",
            post(secret_import).layer(axum::extract::DefaultBodyLimit::max(
                localshelld_sync_protocol::MAX_BODY_BYTES,
            )),
        )
        .route("/v1/admin/sync", post(sync))
        .route("/v1/admin/telegram", post(telegram))
        .route("/v1/admin/password", post(password))
        .route("/v1/admin/totp/start", post(totp_start))
        .route("/v1/admin/totp/finish", post(totp_finish))
        .route("/v1/admin/totp/remove", post(totp_remove))
        .route("/v1/admin/agents", get(agents))
        .route("/v1/admin/agents/policy", post(agent_policy))
        .route("/v1/admin/logs", get(logs))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            audit_request,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            reject_paired_privileged,
        ))
}

/// A paired privileged daemon has no management surface of its own: the single
/// management page at the user daemon reaches it over the bridge.
async fn managed_by_user_daemon(state: &AppState) -> Result<(), ApiError> {
    if state.mode != DaemonMode::Privileged {
        return Ok(());
    }
    match state.settings.lock().await.saved.pairing.as_ref() {
        Some(pairing) => Err(ApiError::conflict(format!(
            "this daemon is managed from the user management page at http://localhost:{}/",
            pairing.peer.port()
        ))),
        None => Ok(()),
    }
}

async fn reject_paired_privileged(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    match managed_by_user_daemon(&state).await {
        Ok(()) => next.run(request).await,
        Err(error) => axum::response::IntoResponse::into_response(error),
    }
}

async fn audit_request(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    let method = request.method().to_string();
    let response = next.run(request).await;
    state
        .audit(
            "management.request",
            json!({"path":path,"method":method,"status":response.status().as_u16()}),
        )
        .await;
    response
}

#[derive(Deserialize)]
struct LogQuery {
    kind: crate::logs::Kind,
    before: Option<u64>,
    limit: Option<usize>,
    /// `privileged` reads the paired root/Administrator daemon's records.
    #[serde(default)]
    daemon: Option<DaemonMode>,
}

async fn logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::bad_request(
            "log page limit must be between 1 and 100",
        ));
    }
    if query.daemon == Some(DaemonMode::Privileged) && state.mode == DaemonMode::User {
        let pairing = state.pairing().await?;
        return bridge::call(
            &pairing,
            DaemonMode::Privileged,
            bridge::Request::LogsPage {
                kind: query.kind,
                before: query.before,
                limit,
            },
            Some(Duration::from_secs(15)),
            4 * 1024 * 1024,
        )
        .await
        .map(Json);
    }
    state
        .logs
        .page(query.kind, query.before, limit)
        .await
        .map(Json)
        .map_err(|_| ApiError::internal("could not read local log"))
}

async fn secrets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    Ok(Json(json!({"keys":state.secret_keys().await?})))
}

async fn secret_export(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    owner(&state, &headers).await?;
    Ok((
        [(
            "content-disposition",
            "attachment; filename=localshelld-credentials.json",
        )],
        Json(state.export_secrets().await?),
    ))
}

async fn secret_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(file): Json<localshelld_sync_protocol::CredentialFile>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    state.import_secrets(file).await?;
    Ok(Json(json!({"imported":true})))
}

async fn sync(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(action): Json<crate::sync::Action>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    Ok(Json(state.sync_action(action).await?))
}

async fn telegram(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(action): Json<crate::telegram::Action>,
) -> Result<Json<Value>, ApiError> {
    let owner = owner(&state, &headers).await?;
    Ok(Json(state.telegram_action(&owner, action).await?))
}

#[derive(Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
struct SecretPut {
    key: String,
    value: String,
}

async fn secret_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SecretPut>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    state.put_secret(&body.key, &body.value).await?;
    Ok(Json(json!({"saved":true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretKey {
    key: String,
}

async fn secret_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SecretKey>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    state.remove_secret(&body.key).await?;
    Ok(Json(json!({"removed":true})))
}

#[derive(Deserialize)]
struct Login {
    credential: Credential,
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<Login>,
) -> Result<Json<Value>, ApiError> {
    if matches!(body.credential.kind, auth::CredentialKind::Ticket) {
        let config = state.settings.lock().await.config_path.clone();
        if !config
            .is_some_and(|config| crate::login::consume_ticket(&config, &body.credential.value))
        {
            return Err(AuthError::InvalidCredential.into());
        }
        state
            .audit("management.login", json!({"method":"ticket"}))
            .await;
    } else if state.mode == DaemonMode::User {
        // There is no static password: the owner's backend passkey signs in.
        return Err(ApiError::bad_request(
            "sign in with the owner's passkey; run `trustd login-url` if the backend is unreachable",
        ));
    } else {
        state.authenticate(&body.credential).await?;
    }
    Ok(Json(state.management.lock().await.session()?))
}

/// Ask the owner's passkey, on the backend, to sign a one-time login
/// statement this daemon generated. The daemon verifies the signature itself.
async fn passkey_login_start(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    if state.mode != DaemonMode::User {
        return Err(ApiError::bad_request(
            "this daemon signs in with its Master Password",
        ));
    }
    let device_id = state.remote.joined_device_id().await.ok_or_else(|| {
        ApiError::unavailable(
            "this device has not joined a backend; run `trustd join` or `trustd login-url`",
        )
    })?;
    let statement = Statement::ManagementLogin {
        device_id,
        nonce: strong_handle(),
        issued_at: Utc::now().timestamp(),
    }
    .to_text();
    let started = state
        .backend_request(
            "signatures",
            json!({"statement":statement}),
            Duration::from_secs(15),
        )
        .await?;
    let backend = state.backend_url().await?;
    let verification = started["verification_uri"].as_str().unwrap_or_default();
    let (Some(id), Some(code)) = (started["id"].as_str(), started["code"].as_str()) else {
        return Err(ApiError::unavailable("invalid backend signature response"));
    };
    // Never open an arbitrary backend-supplied URL.
    if reqwest::Url::parse(verification)
        .map(|url| url.origin().ascii_serialization())
        .ok()
        .as_deref()
        != Some(backend.as_str())
    {
        return Err(ApiError::unavailable("invalid backend verification URL"));
    }
    let login_id = strong_handle();
    let mut management = state.management.lock().await;
    management.prune();
    if management.logins.len() >= 16 {
        return Err(ApiError::unavailable(
            "too many pending logins; retry later",
        ));
    }
    management.logins.insert(
        login_id.clone(),
        PendingLogin {
            statement,
            backend_id: id.into(),
            expires_at: Utc::now().timestamp() + CHALLENGE_SECONDS,
        },
    );
    Ok(Json(json!({
        "login_id":login_id,"verification_uri":verification,"user_code":code,
        "expires_in":CHALLENGE_SECONDS,"interval":2
    })))
}

#[derive(Deserialize)]
struct LoginPoll {
    login_id: String,
}
async fn passkey_login_poll(
    State(state): State<AppState>,
    Json(body): Json<LoginPoll>,
) -> Result<Json<Value>, ApiError> {
    let (statement, backend_id) = {
        let mut management = state.management.lock().await;
        management.prune();
        let login = management
            .logins
            .get(&body.login_id)
            .ok_or_else(|| ApiError::bad_request("login request expired; start again"))?;
        (login.statement.clone(), login.backend_id.clone())
    };
    let result = state
        .backend_request(
            "signatures/poll",
            json!({"id":backend_id}),
            Duration::from_secs(15),
        )
        .await?;
    if result["pending"] == true {
        return Ok(Json(json!({"pending":true})));
    }
    let assertion = serde_json::from_value(result["assertion"].clone())
        .map_err(|_| ApiError::unavailable("invalid backend signature"))?;
    // Our own statement text, verified against the owner pinned at join.
    let signed = SignedStatement {
        statement,
        assertion,
    };
    match state.remote.verify_owner_statement(&signed).await {
        Ok(Statement::ManagementLogin { issued_at, .. })
            if Utc::now().timestamp() - issued_at <= CHALLENGE_SECONDS => {}
        _ => return Err(AuthError::InvalidCredential.into()),
    }
    let mut management = state.management.lock().await;
    management.logins.remove(&body.login_id);
    let session = management.session()?;
    drop(management);
    state
        .audit("management.login", json!({"method":"owner_passkey"}))
        .await;
    Ok(Json(session))
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let owner = owner(&state, &headers).await?;
    state.management.lock().await.sessions.remove(&owner);
    Ok(Json(json!({"logged_out":true})))
}

async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let settings = state.settings.lock().await;
    Ok(Json(json!({
        "daemon":state.mode, "totp_enabled":settings.config.totp_secret.is_some(),
        "paired_peer":settings.saved.pairing.as_ref().map(|pair| pair.peer.to_string()),
        "desktop_available":state.consent.is_some()
    })))
}

#[derive(Deserialize)]
struct Password {
    password: String,
}

async fn password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Password>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(body.password.as_bytes()))
        .await
        .map_err(|_| ApiError::internal("password worker failed"))?
        .map_err(|_| ApiError::internal("password hashing failed"))?;
    let mut settings = state.settings.lock().await;
    let mut config = settings.config.clone();
    config.password_hash = hash.clone();
    settings.save_config(config).await?;
    state.auth.lock().await.replace_password(hash);
    *state.management.lock().await = Management::default();
    Ok(Json(json!({"updated":true,"reauthenticate":true})))
}

async fn totp_start(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let owner = owner(&state, &headers).await?;
    let secret = auth::generate_totp_secret();
    let mut totp =
        auth::create_totp(&secret).map_err(|_| ApiError::internal("TOTP enrollment failed"))?;
    if state.mode == DaemonMode::User {
        totp.account_name = "local-user".into();
    }
    let id = state
        .management
        .lock()
        .await
        .insert(Pending::Totp { owner, secret })?;
    Ok(Json(
        json!({"enrollment_id":id,"secret":totp.get_secret_base32(),"uri":totp.get_url(),"expires_in":CHALLENGE_SECONDS}),
    ))
}

#[derive(Deserialize)]
struct TotpFinish {
    enrollment_id: String,
    code: String,
}

async fn totp_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TotpFinish>,
) -> Result<Json<Value>, ApiError> {
    let session = owner(&state, &headers).await?;
    let Pending::Totp { owner, secret } =
        state.management.lock().await.take(&body.enrollment_id)?;
    if owner != session {
        return Err(AuthError::InvalidCredential.into());
    }
    let totp =
        auth::create_totp(&secret).map_err(|_| ApiError::internal("invalid TOTP enrollment"))?;
    if !totp.check_current(body.code.trim()).unwrap_or(false) {
        return Err(ApiError::bad_request(
            "incorrect TOTP code; start enrollment again",
        ));
    }
    let mut settings = state.settings.lock().await;
    let path = settings
        .config_path
        .clone()
        .ok_or_else(|| ApiError::unavailable("configuration storage unavailable"))?;
    let key = tokio::task::spawn_blocking(move || load_or_create_seal_key(&seal_key_path(&path)))
        .await
        .map_err(|_| ApiError::internal("seal key worker failed"))?
        .map_err(|_| ApiError::internal("seal key unavailable"))?;
    let mut config = settings.config.clone();
    config.totp_secret =
        Some(seal(&secret, &key).map_err(|_| ApiError::internal("TOTP encryption failed"))?);
    settings.save_config(config).await?;
    state.auth.lock().await.replace_totp(Some(secret));
    Ok(Json(json!({"enabled":true})))
}

async fn totp_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let mut settings = state.settings.lock().await;
    let mut config = settings.config.clone();
    config.totp_secret = None;
    settings.save_config(config).await?;
    state.auth.lock().await.replace_totp(None);
    Ok(Json(json!({"enabled":false})))
}

async fn agents(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    Ok(Json(
        json!({"agents":state.agent_list().await?, "process_names":crate::agents::PROCESS_NAMES}),
    ))
}

async fn agent_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(change): Json<localshelld_telegram::agents::Change>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    state.set_agent_policy(change).await?;
    Ok(Json(json!({"saved":true})))
}

#[cfg(test)]
mod tests;
