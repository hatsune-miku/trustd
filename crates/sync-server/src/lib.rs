#![forbid(unsafe_code)]
mod storage;
mod telegram;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use localshelld_sync_protocol::{
    DeviceJoin, DeviceStart, Joined, MAX_BODY_BYTES, Replace, RosterResponse, Snapshot, Tokens,
    owner::{self, Owner, OwnerKey, Roster, SignedStatement, Statement},
    validate_credentials,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use storage::{Session, Store, StoredPasskey, digest, random};
use tokio::sync::{Mutex, watch};
use webauthn_rs::prelude::*;

const ACCESS_SECONDS: i64 = 300;
const REFRESH_SECONDS: i64 = 30 * 86400;
const CHALLENGE_SECONDS: i64 = 300;
const MAX_PENDING: usize = 128;
const MAX_SESSIONS: usize = 128;
const MAX_STATEMENTS: usize = 4096;
/// Allowed difference between a joining device's clock and the service's.
const JOIN_CLOCK_SKEW: i64 = 600;

#[derive(Debug)]
struct Error(StatusCode, &'static str);
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
fn bad(message: &'static str) -> Error {
    Error(StatusCode::BAD_REQUEST, message)
}
fn denied() -> Error {
    Error(
        StatusCode::UNAUTHORIZED,
        "authentication required or expired",
    )
}
fn internal() -> Error {
    Error(StatusCode::INTERNAL_SERVER_ERROR, "state operation failed")
}
type Result<T> = std::result::Result<T, Error>;

enum Challenge {
    Register {
        state: PasskeyRegistration,
        name: String,
        bootstrap: bool,
        session_id: Option<String>,
    },
    Authenticate(PasskeyAuthentication),
}
struct Device {
    user_code: String,
    name: String,
    device_id: String,
    /// The device's own approval statement, exactly as the owner signs it.
    statement: String,
    expires_at: i64,
    last_poll: i64,
    approval: Option<SignedStatement>,
}
/// A device asking the owner to sign one of its own statements, such as a
/// management login. The device verifies the returned assertion itself.
struct SignatureRequest {
    code: String,
    device_id: String,
    device_name: String,
    statement: String,
    expires_at: i64,
    assertion: Option<PublicKeyCredential>,
}
struct Pending {
    challenges: HashMap<String, (i64, Challenge)>,
    devices: HashMap<String, Device>,
    signatures: HashMap<String, SignatureRequest>,
    minute: i64,
    starts: u32,
}
impl Pending {
    fn prune(&mut self) {
        let now = Utc::now().timestamp();
        self.challenges.retain(|_, (expiry, _)| *expiry > now);
        self.devices.retain(|_, device| device.expires_at > now);
        self.signatures
            .retain(|_, request| request.expires_at > now);
    }
    fn allow_start(&mut self) -> Result<()> {
        self.prune();
        let minute = Utc::now().timestamp() / 60;
        if minute != self.minute {
            self.minute = minute;
            self.starts = 0;
        }
        self.starts += 1;
        if self.starts > 120
            || self.challenges.len() + self.devices.len() + self.signatures.len() >= MAX_PENDING
        {
            return Err(Error(
                StatusCode::TOO_MANY_REQUESTS,
                "too many pending requests; retry later",
            ));
        }
        Ok(())
    }
    fn take(&mut self, id: &str) -> Result<Challenge> {
        self.prune();
        self.challenges
            .remove(id)
            .map(|(_, value)| value)
            .ok_or_else(|| bad("challenge expired or already used"))
    }
}

#[derive(Clone)]
pub struct App {
    store: Arc<Mutex<Store>>,
    pending: Arc<Mutex<Pending>>,
    webauthn: Arc<Webauthn>,
    origin: String,
    rp_id: String,
    revision: watch::Sender<u64>,
    telegram: localshelld_telegram::Service,
    telegram_config_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Claims {
    sub: String,
    sid: String,
    iss: String,
    aud: String,
    exp: i64,
    iat: i64,
}

impl App {
    pub fn open(directory: &Path, origin: &str) -> anyhow::Result<Self> {
        let url = Url::parse(origin)?;
        anyhow::ensure!(
            url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none(),
            "origin must contain only scheme, host and optional port"
        );
        anyhow::ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http" && url.host_str() == Some("localhost")),
            "HTTPS is required, except http://localhost for development"
        );
        let origin = url.origin().ascii_serialization();
        let rp_id = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("missing host"))?
            .to_owned();
        let webauthn = WebauthnBuilder::new(&rp_id, &url)?
            .rp_name("localshelld sync")
            .build()?;
        let store = Store::open(directory)?;
        let (revision, _) = watch::channel(store.saved.vault.revision);
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            webauthn: Arc::new(webauthn),
            origin,
            rp_id,
            pending: Arc::new(Mutex::new(Pending {
                challenges: HashMap::new(),
                devices: HashMap::new(),
                signatures: HashMap::new(),
                minute: 0,
                starts: 0,
            })),
            revision,
            telegram: localshelld_telegram::Service::default(),
            telegram_config_lock: Arc::new(Mutex::new(())),
        })
    }
    /// Set the genesis owner keys without a WebAuthn registration ceremony.
    #[cfg(feature = "testing")]
    pub async fn initialize_owner_for_tests(&self, keys: Vec<OwnerKey>) {
        let mut store = self.store.lock().await;
        let mut next = store.saved.clone();
        next.genesis = keys;
        store.commit(next).expect("test state commit");
    }

    /// The owner as devices pin it: this relying party and the genesis keys.
    fn owner(&self, saved: &storage::Saved) -> Owner {
        Owner {
            rp_id: self.rp_id.clone(),
            origin: self.origin.clone(),
            keys: saved.genesis.clone(),
        }
    }
    /// Derived exactly as every device derives it, from verified statements.
    fn roster(&self, saved: &storage::Saved) -> Roster {
        owner::roster(&self.owner(saved), &saved.statements)
    }
    fn authenticate(&self, store: &Store, headers: &HeaderMap) -> Result<Claims> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(denied)?;
        self.claims(store, token)
    }
    fn claims(&self, store: &Store, token: &str) -> Result<Claims> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.set_audience(&["localshelld-sync"]);
        validation.set_issuer(&[&self.origin]);
        validation.set_required_spec_claims(&["exp", "sub", "iss", "aud"]);
        let claims = jsonwebtoken::decode::<Claims>(
            token,
            &DecodingKey::from_secret(store.saved.jwt_key.as_bytes()),
            &validation,
        )
        .map_err(|_| denied())?
        .claims;
        if claims.sub != store.saved.user_id.to_string()
            || store
                .saved
                .sessions
                .get(&claims.sid)
                .is_none_or(|s| s.expires_at <= Utc::now().timestamp())
        {
            return Err(denied());
        }
        Ok(claims)
    }
    fn browser(&self, store: &Store, headers: &HeaderMap) -> Result<Claims> {
        let claims = self.authenticate(store, headers)?;
        if !store.saved.sessions[&claims.sid].browser {
            return Err(Error(
                StatusCode::FORBIDDEN,
                "sign in with a passkey in the browser",
            ));
        }
        Ok(claims)
    }
    fn tokens(
        &self,
        saved: &storage::Saved,
        sid: &str,
        refresh_token: String,
        refresh_expires_at: i64,
    ) -> Result<Tokens> {
        let now = Utc::now().timestamp();
        let expires_at = (now + ACCESS_SECONDS).min(refresh_expires_at);
        let claims = Claims {
            sub: saved.user_id.to_string(),
            sid: sid.into(),
            iss: self.origin.clone(),
            aud: "localshelld-sync".into(),
            exp: expires_at,
            iat: now,
        };
        let access_token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(saved.jwt_key.as_bytes()),
        )
        .map_err(|_| internal())?;
        Ok(Tokens {
            access_token,
            refresh_token,
            expires_at,
            refresh_expires_at,
        })
    }
    fn new_session(
        &self,
        store: &mut Store,
        name: String,
        browser: bool,
        device: Option<String>,
    ) -> Result<Tokens> {
        let now = Utc::now().timestamp();
        let mut next = store.saved.clone();
        next.sessions.retain(|_, session| session.expires_at > now);
        if next.sessions.len() >= MAX_SESSIONS {
            return Err(bad("too many trusted sessions; revoke a session first"));
        }
        let sid = random();
        // Browser sessions are deliberately short-lived and cannot be renewed:
        // every trust-management action is backed by a recent passkey ceremony.
        let expires = now
            + if browser {
                ACCESS_SECONDS
            } else {
                REFRESH_SECONDS
            };
        let refresh_token = if browser {
            String::new()
        } else {
            format!("{sid}.{}", random())
        };
        next.sessions.insert(
            sid.clone(),
            Session {
                name,
                browser,
                created_at: now,
                expires_at: expires,
                refresh_hash: digest(&refresh_token),
                used_refresh: vec![],
                device,
            },
        );
        let tokens = self.tokens(&next, &sid, refresh_token, expires)?;
        store.commit(next).map_err(|_| internal())?;
        Ok(tokens)
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route(
            "/health",
            get(|| async { Json(json!({"service":"localshelld-sync", "protocol":1})) }),
        )
        .route("/sync/v1/info", get(info))
        .route("/sync/v1/passkeys/register/start", post(register_start))
        .route("/sync/v1/passkeys/register/finish", post(register_finish))
        .route(
            "/sync/v1/passkeys/authenticate/start",
            post(authenticate_start),
        )
        .route(
            "/sync/v1/passkeys/authenticate/finish",
            post(authenticate_finish),
        )
        .route("/sync/v1/session/refresh", post(refresh))
        .route("/sync/v1/session/revoke", post(revoke))
        .route("/sync/v1/sessions", get(sessions))
        .route("/sync/v1/device/start", post(device_start))
        .route("/sync/v1/device/inspect", post(device_inspect))
        .route("/sync/v1/device/approve", post(device_approve))
        .route("/sync/v1/device/poll", post(device_poll))
        .route("/sync/v1/roster", get(roster))
        .route("/sync/v1/statements/prepare", post(statement_prepare))
        .route("/sync/v1/statements", post(statement_submit))
        .route("/sync/v1/signatures", post(signature_start))
        .route("/sync/v1/signatures/inspect", post(signature_inspect))
        .route("/sync/v1/signatures/approve", post(signature_approve))
        .route("/sync/v1/signatures/poll", post(signature_poll))
        .route("/sync/v1/vault", get(vault_get).put(vault_put))
        .route("/sync/v1/telegram", post(telegram::control))
        .route("/sync/v1/telegram/request", post(telegram::create))
        .route("/sync/v1/telegram/record", post(telegram::record))
        .route("/sync/v1/telegram/poll", post(telegram::poll))
        .route("/sync/v1/telegram/finish", post(telegram::finish))
        .route("/sync/v1/telegram/agents", post(telegram::agents))
        .route("/sync/v1/events", get(events))
        .merge(trustd_remote::relay::router(relay_admission(app.clone())))
        .fallback(assets)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(app.clone(), security))
        .with_state(app)
}

async fn security(State(app): State<App>, request: Request, next: Next) -> Response {
    if request
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|v| v.to_str().ok() != Some(app.origin.as_str()))
    {
        return Error(StatusCode::FORBIDDEN, "origin is not allowed").into_response();
    }
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}

async fn info(State(app): State<App>) -> Json<Value> {
    let store = app.store.lock().await;
    Json(
        json!({"protocol":1,"features":["telegram-v1"],"initialized":!store.saved.passkeys.is_empty(),"origin":app.origin}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterStart {
    name: String,
    #[serde(default)]
    bootstrap_code: String,
}
async fn register_start(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<RegisterStart>,
) -> Result<Json<Value>> {
    if body.name.trim().is_empty() || body.name.len() > 80 {
        return Err(bad("passkey name must contain 1–80 bytes"));
    }
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    let store = app.store.lock().await;
    let bootstrap = store.saved.passkeys.is_empty();
    let session_id = if bootstrap {
        if store.saved.bootstrap_hash.as_deref() != Some(digest(&body.bootstrap_code).as_str()) {
            return Err(denied());
        }
        None
    } else {
        Some(app.browser(&store, &headers)?.sid)
    };
    if store.saved.passkeys.len() >= 32 {
        return Err(bad("at most 32 passkeys can be registered"));
    }
    let exclude = Some(
        store
            .saved
            .passkeys
            .iter()
            .map(|key| key.key.cred_id().clone())
            .collect(),
    );
    let (options, state) = app
        .webauthn
        .start_passkey_registration(store.saved.user_id, "owner", "localshelld owner", exclude)
        .map_err(|_| bad("could not start passkey registration"))?;
    let id = random();
    pending.challenges.insert(
        id.clone(),
        (
            Utc::now().timestamp() + CHALLENGE_SECONDS,
            Challenge::Register {
                state,
                name: body.name.trim().into(),
                bootstrap,
                session_id,
            },
        ),
    );
    Ok(Json(json!({"challenge_id":id,"options":options})))
}

#[derive(Deserialize)]
struct RegisterFinish {
    challenge_id: String,
    credential: RegisterPublicKeyCredential,
}
async fn register_finish(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinish>,
) -> Result<Json<Value>> {
    let challenge = app.pending.lock().await.take(&body.challenge_id)?;
    let Challenge::Register {
        state,
        name,
        bootstrap,
        session_id,
    } = challenge
    else {
        return Err(bad("wrong ceremony"));
    };
    let key = app
        .webauthn
        .finish_passkey_registration(&body.credential, &state)
        .map_err(|_| bad("passkey registration failed"))?;
    let mut store = app.store.lock().await;
    if bootstrap {
        if !store.saved.passkeys.is_empty() {
            return Err(Error(StatusCode::CONFLICT, "already initialized"));
        }
    } else if Some(app.browser(&store, &headers)?.sid) != session_id {
        return Err(denied());
    }
    if store.saved.passkeys.len() >= 32
        || store
            .saved
            .passkeys
            .iter()
            .any(|existing| existing.key.cred_id() == key.cred_id())
    {
        return Err(bad("passkey already registered or limit reached"));
    }
    let mut next = store.saved.clone();
    next.bootstrap_hash = None;
    let owner_key = OwnerKey::from(&key);
    if bootstrap {
        // The initializing passkey is the base every device pins.
        next.genesis = vec![owner_key.clone()];
    }
    next.passkeys.push(StoredPasskey {
        id: random(),
        name,
        key,
    });
    store.commit(next).map_err(|_| internal())?;
    store.remove_bootstrap_file();
    let mut result = json!(app.new_session(&mut store, "Browser".into(), true, None)?);
    if !bootstrap {
        // Devices trust a later passkey only after a trusted one endorses it.
        result["endorse"] = app.prepare(
            &store.saved,
            &Statement::PasskeyAdd {
                key: owner_key,
                issued_at: Utc::now().timestamp(),
            },
        );
    }
    Ok(Json(result))
}

async fn authenticate_start(State(app): State<App>) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    let store = app.store.lock().await;
    if store.saved.passkeys.is_empty() {
        return Err(bad("initialize the service first"));
    }
    let keys: Vec<_> = store
        .saved
        .passkeys
        .iter()
        .map(|key| key.key.clone())
        .collect();
    let (options, state) = app
        .webauthn
        .start_passkey_authentication(&keys)
        .map_err(|_| bad("could not start authentication"))?;
    let id = random();
    pending.challenges.insert(
        id.clone(),
        (
            Utc::now().timestamp() + CHALLENGE_SECONDS,
            Challenge::Authenticate(state),
        ),
    );
    Ok(Json(json!({"challenge_id":id,"options":options})))
}

#[derive(Deserialize)]
struct AuthenticateFinish {
    challenge_id: String,
    credential: PublicKeyCredential,
}
async fn authenticate_finish(
    State(app): State<App>,
    Json(body): Json<AuthenticateFinish>,
) -> Result<Json<Tokens>> {
    let Challenge::Authenticate(state) = app.pending.lock().await.take(&body.challenge_id)? else {
        return Err(bad("wrong ceremony"));
    };
    let auth = app
        .webauthn
        .finish_passkey_authentication(&body.credential, &state)
        .map_err(|_| denied())?;
    let mut store = app.store.lock().await;
    let mut next = store.saved.clone();
    let key = next
        .passkeys
        .iter_mut()
        .find(|key| key.key.cred_id() == auth.cred_id())
        .ok_or_else(denied)?;
    key.key.update_credential(&auth);
    store.commit(next).map_err(|_| internal())?;
    Ok(Json(app.new_session(
        &mut store,
        "Browser".into(),
        true,
        None,
    )?))
}

#[derive(Deserialize)]
struct Id {
    id: String,
}
#[derive(Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
struct Refresh {
    refresh_token: String,
}
async fn refresh(State(app): State<App>, Json(body): Json<Refresh>) -> Result<Json<Tokens>> {
    let (sid, _) = body.refresh_token.split_once('.').ok_or_else(denied)?;
    let hash = digest(&body.refresh_token);
    let mut store = app.store.lock().await;
    let mut next = store.saved.clone();
    let session = next.sessions.get_mut(sid).ok_or_else(denied)?;
    if session.browser || session.expires_at <= Utc::now().timestamp() {
        return Err(denied());
    }
    if session.used_refresh.contains(&hash) {
        next.sessions.remove(sid);
        store.commit(next).map_err(|_| internal())?;
        return Err(denied());
    }
    if session.refresh_hash != hash {
        return Err(denied());
    }
    if session.used_refresh.len() >= 12_000 {
        return Err(bad("session renewal limit reached; sign in again"));
    }
    let refresh_token = format!("{sid}.{}", random());
    session.used_refresh.push(hash);
    session.refresh_hash = digest(&refresh_token);
    let expires = session.expires_at;
    let result = app.tokens(&next, sid, refresh_token, expires)?;
    store.commit(next).map_err(|_| internal())?;
    Ok(Json(result))
}

async fn sessions(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    let store = app.store.lock().await;
    app.browser(&store, &headers)?;
    let trusted = app.roster(&store.saved).keys;
    let is_trusted = |key: &StoredPasskey| {
        let key = OwnerKey::from(&key.key);
        trusted.iter().any(|t| t.credential_id == key.credential_id)
    };
    Ok(Json(
        json!({"passkeys":store.saved.passkeys.iter().map(|key| json!({"id":key.id,"name":key.name,"trusted":is_trusted(key)})).collect::<Vec<_>>(),
        "sessions":store.saved.sessions.iter().filter(|(_,s)| !s.browser && s.expires_at > Utc::now().timestamp()).map(|(id,s)| json!({"id":id,"name":s.name,"created_at":s.created_at,"expires_at":s.expires_at})).collect::<Vec<_>>()}),
    ))
}
async fn revoke(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Id>,
) -> Result<Json<Value>> {
    let mut store = app.store.lock().await;
    let claims = app.authenticate(&store, &headers)?;
    let target = if body.id.is_empty() {
        &claims.sid
    } else {
        &body.id
    };
    if target != &claims.sid {
        app.browser(&store, &headers)?;
    }
    let mut next = store.saved.clone();
    next.sessions.remove(target);
    store.commit(next).map_err(|_| internal())?;
    // Wake long polls so a revoked device is rejected without waiting for timeout.
    app.revision.send_replace(store.saved.vault.revision);
    Ok(Json(json!({"revoked":true})))
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl App {
    /// Everything the approval page needs to have a trusted owner passkey
    /// sign `statement`: its exact text and the derived WebAuthn challenge.
    fn prepare(&self, saved: &storage::Saved, statement: &Statement) -> Value {
        let text = statement.to_text();
        json!({
            "statement":text,
            "challenge":b64(&owner::challenge(&text)),
            "rp_id":self.rp_id,
            "allow_credentials":self.roster(saved).keys.iter()
                .map(|key| b64(key.credential_id.as_ref())).collect::<Vec<_>>(),
        })
    }
}

/// A device joins by submitting its own approval statement; the owner signs
/// exactly that text on the approval page, so every device can verify it.
async fn device_start(
    State(app): State<App>,
    Json(body): Json<DeviceJoin>,
) -> Result<Json<DeviceStart>> {
    let Ok(Statement::DeviceApprove {
        device_id,
        name,
        issued_at,
        ..
    }) = Statement::parse(&body.statement)
    else {
        return Err(bad("join requires a valid device approval statement"));
    };
    if (issued_at - Utc::now().timestamp()).abs() > JOIN_CLOCK_SKEW {
        return Err(bad(
            "device clock differs from the service by more than 10 minutes",
        ));
    }
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    {
        let store = app.store.lock().await;
        if store.saved.genesis.is_empty() {
            return Err(bad("initialize the service first"));
        }
        if app.roster(&store.saved).revoked.contains(&device_id) {
            return Err(Error(
                StatusCode::FORBIDDEN,
                "this device identity was revoked; reset it before joining again",
            ));
        }
    }
    pending
        .devices
        .retain(|_, device| device.device_id != device_id);
    let device_code = random();
    let user_code = uuid::Uuid::new_v4().simple().to_string()[..12].to_uppercase();
    pending.devices.insert(
        digest(&device_code),
        Device {
            user_code: user_code.clone(),
            name,
            device_id,
            statement: body.statement.clone(),
            expires_at: Utc::now().timestamp() + 600,
            last_poll: 0,
            approval: None,
        },
    );
    Ok(Json(DeviceStart {
        device_code,
        user_code: user_code.clone(),
        verification_uri: format!("{}/#join={user_code}", app.origin),
        expires_in: 600,
        interval: 3,
    }))
}

#[derive(Deserialize)]
struct UserCode {
    user_code: String,
}
/// Public by design: the user code is the capability to view one pending
/// request, and approving it still needs the owner's passkey signature.
async fn device_inspect(State(app): State<App>, Json(body): Json<UserCode>) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    let device = pending
        .devices
        .values()
        .find(|d| d.user_code == body.user_code)
        .ok_or_else(|| bad("device request expired"))?;
    let store = app.store.lock().await;
    let Ok(statement) = Statement::parse(&device.statement) else {
        return Err(internal());
    };
    let mut value = app.prepare(&store.saved, &statement);
    // The exact stored text, not a re-serialization, is what gets signed.
    value["statement"] = json!(device.statement);
    value["challenge"] = json!(b64(&owner::challenge(&device.statement)));
    value["name"] = json!(device.name);
    value["device_id"] = json!(device.device_id);
    value["user_code"] = json!(device.user_code);
    value["approved"] = json!(device.approval.is_some());
    Ok(Json(value))
}

#[derive(Deserialize)]
struct Approve {
    user_code: String,
    assertion: PublicKeyCredential,
}
async fn device_approve(State(app): State<App>, Json(body): Json<Approve>) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.prune();
    let device = pending
        .devices
        .values_mut()
        .find(|d| d.user_code == body.user_code)
        .ok_or_else(|| bad("device request expired"))?;
    if device.approval.is_some() {
        return Ok(Json(json!({"approved":true})));
    }
    let signed = SignedStatement {
        statement: device.statement.clone(),
        assertion: body.assertion,
    };
    let mut store = app.store.lock().await;
    let roster = app.roster(&store.saved);
    signed
        .verify(&app.owner(&store.saved), &roster.keys)
        .map_err(|_| denied())?;
    if store.saved.statements.len() >= MAX_STATEMENTS {
        return Err(bad("statement limit reached"));
    }
    let mut next = store.saved.clone();
    next.statements.push(signed.clone());
    store.commit(next).map_err(|_| internal())?;
    device.approval = Some(signed);
    Ok(Json(json!({"approved":true})))
}

#[derive(Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
struct DeviceCode {
    device_code: String,
}
async fn device_poll(State(app): State<App>, Json(body): Json<DeviceCode>) -> Result<Response> {
    let mut pending = app.pending.lock().await;
    pending.prune();
    let hash = digest(&body.device_code);
    let device = pending
        .devices
        .get_mut(&hash)
        .ok_or_else(|| bad("device request expired or already used"))?;
    let now = Utc::now().timestamp();
    if now - device.last_poll < 2 {
        return Err(Error(
            StatusCode::TOO_MANY_REQUESTS,
            "poll no more than once every 3 seconds",
        ));
    }
    device.last_poll = now;
    let Some(approval) = device.approval.clone() else {
        return Ok((StatusCode::ACCEPTED, Json(json!({"pending":true}))).into_response());
    };
    let (name, id) = (device.name.clone(), device.device_id.clone());
    let mut store = app.store.lock().await;
    let tokens = app.new_session(&mut store, name, false, Some(id))?;
    pending.devices.remove(&hash);
    Ok(Json(Joined {
        tokens,
        owner: app.owner(&store.saved),
        approval,
        statements: store.saved.statements.clone(),
    })
    .into_response())
}

async fn roster(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    let store = app.store.lock().await;
    app.authenticate(&store, &headers)?;
    let roster = app.roster(&store.saved);
    let mut value = json!(RosterResponse {
        owner: app.owner(&store.saved),
        statements: store.saved.statements.clone(),
    });
    // Display aid for the owner's page; devices derive the roster themselves.
    value["devices"] = json!(roster.devices);
    value["rules"] = json!(roster.rules);
    value["trusted_keys"] = json!(
        roster
            .keys
            .iter()
            .map(|key| b64(key.credential_id.as_ref()))
            .collect::<Vec<_>>()
    );
    Ok(Json(value))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Prepare {
    DeviceRevoke { device_id: String },
    RuleSet { rules: Vec<owner::Rule> },
    PasskeyAdd { id: String },
    PasskeyRemove { id: String },
}
async fn statement_prepare(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Prepare>,
) -> Result<Json<Value>> {
    let store = app.store.lock().await;
    app.browser(&store, &headers)?;
    let issued_at = Utc::now().timestamp();
    let statement = match body {
        Prepare::DeviceRevoke { device_id } => Statement::DeviceRevoke {
            device_id,
            issued_at,
        },
        Prepare::RuleSet { rules } => {
            let statement = Statement::RuleSet { rules, issued_at };
            Statement::parse(&statement.to_text()).map_err(|error| bad(error.0))?;
            statement
        }
        Prepare::PasskeyAdd { id } => Statement::PasskeyAdd {
            key: OwnerKey::from(
                &store
                    .saved
                    .passkeys
                    .iter()
                    .find(|key| key.id == id)
                    .ok_or_else(|| bad("passkey not found"))?
                    .key,
            ),
            issued_at,
        },
        Prepare::PasskeyRemove { id } => Statement::PasskeyRemove {
            credential_id: store
                .saved
                .passkeys
                .iter()
                .find(|key| key.id == id)
                .ok_or_else(|| bad("passkey not found"))?
                .key
                .cred_id()
                .as_ref()
                .to_vec()
                .into(),
            issued_at,
        },
    };
    Ok(Json(app.prepare(&store.saved, &statement)))
}

/// Accept an owner-signed statement. The signature is the authorization:
/// no session can make the service record a statement the owner did not sign.
async fn statement_submit(
    State(app): State<App>,
    Json(signed): Json<SignedStatement>,
) -> Result<Json<Value>> {
    let mut store = app.store.lock().await;
    let roster = app.roster(&store.saved);
    let statement = signed
        .verify(&app.owner(&store.saved), &roster.keys)
        .map_err(|_| denied())?;
    if store
        .saved
        .statements
        .iter()
        .any(|existing| existing.statement == signed.statement)
    {
        return Ok(Json(json!({"accepted":true})));
    }
    if store.saved.statements.len() >= MAX_STATEMENTS {
        return Err(bad("statement limit reached"));
    }
    let mut next = store.saved.clone();
    match &statement {
        Statement::DeviceApprove { .. } => {
            return Err(bad("devices join through the approval page"));
        }
        Statement::ManagementLogin { .. } => {
            return Err(bad("login proofs are returned to the device, not recorded"));
        }
        Statement::RuleSet { .. } => {}
        Statement::DeviceRevoke { device_id, .. } => {
            next.sessions
                .retain(|_, session| session.device.as_deref() != Some(device_id.as_str()));
        }
        Statement::PasskeyAdd { key, .. } => {
            if !next
                .passkeys
                .iter()
                .any(|stored| OwnerKey::from(&stored.key).credential_id == key.credential_id)
            {
                return Err(bad("register the passkey before trusting it"));
            }
        }
        Statement::PasskeyRemove { credential_id, .. } => {
            if roster.keys.len() <= 1
                && roster
                    .keys
                    .iter()
                    .any(|k| &k.credential_id == credential_id)
            {
                return Err(bad("the last trusted passkey cannot be removed"));
            }
            next.passkeys
                .retain(|stored| stored.key.cred_id().as_ref() != credential_id.as_ref());
            if next.passkeys.is_empty() {
                return Err(bad("the last passkey cannot be removed"));
            }
        }
    }
    next.statements.push(signed);
    store.commit(next).map_err(|_| internal())?;
    // Wake long polls so revoked devices are rejected promptly.
    app.revision.send_replace(store.saved.vault.revision);
    Ok(Json(json!({"accepted":true})))
}

/// The roster device behind an authenticated device session.
fn session_device(app: &App, store: &Store, headers: &HeaderMap) -> Result<(String, String)> {
    let claims = app.authenticate(store, headers)?;
    let forbidden = || Error(StatusCode::FORBIDDEN, "a joined device session is required");
    let device = store.saved.sessions[&claims.sid]
        .device
        .clone()
        .ok_or_else(forbidden)?;
    let name = app
        .roster(&store.saved)
        .devices
        .get(&device)
        .ok_or_else(forbidden)?
        .name
        .clone();
    Ok((device, name))
}

async fn signature_start(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<DeviceJoin>,
) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    let store = app.store.lock().await;
    let (device_id, device_name) = session_device(&app, &store, &headers)?;
    let Ok(Statement::ManagementLogin {
        device_id: signed_for,
        issued_at,
        ..
    }) = Statement::parse(&body.statement)
    else {
        return Err(bad("only management login statements can be requested"));
    };
    if signed_for != device_id {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "a device can only request statements about itself",
        ));
    }
    if (issued_at - Utc::now().timestamp()).abs() > JOIN_CLOCK_SKEW {
        return Err(bad(
            "device clock differs from the service by more than 10 minutes",
        ));
    }
    let id = random();
    let code = uuid::Uuid::new_v4().simple().to_string()[..12].to_uppercase();
    pending.signatures.insert(
        digest(&id),
        SignatureRequest {
            code: code.clone(),
            device_id,
            device_name,
            statement: body.statement.clone(),
            expires_at: Utc::now().timestamp() + 300,
            assertion: None,
        },
    );
    Ok(Json(json!({
        "id":id,"code":code,"verification_uri":format!("{}/#sign={code}", app.origin),
        "expires_in":300,"interval":2
    })))
}

async fn signature_inspect(
    State(app): State<App>,
    Json(body): Json<UserCode>,
) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    let request = pending
        .signatures
        .values()
        .find(|r| r.code == body.user_code)
        .ok_or_else(|| bad("signature request expired"))?;
    let store = app.store.lock().await;
    let Ok(statement) = Statement::parse(&request.statement) else {
        return Err(internal());
    };
    let mut value = app.prepare(&store.saved, &statement);
    value["statement"] = json!(request.statement);
    value["challenge"] = json!(b64(&owner::challenge(&request.statement)));
    value["device_name"] = json!(request.device_name);
    value["device_id"] = json!(request.device_id);
    value["user_code"] = json!(request.code);
    value["purpose"] = json!("management_login");
    value["approved"] = json!(request.assertion.is_some());
    Ok(Json(value))
}

async fn signature_approve(
    State(app): State<App>,
    Json(body): Json<Approve>,
) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.prune();
    let request = pending
        .signatures
        .values_mut()
        .find(|r| r.code == body.user_code)
        .ok_or_else(|| bad("signature request expired"))?;
    let store = app.store.lock().await;
    let signed = SignedStatement {
        statement: request.statement.clone(),
        assertion: body.assertion,
    };
    // The device verifies again; checking here only keeps junk out.
    signed
        .verify(&app.owner(&store.saved), &app.roster(&store.saved).keys)
        .map_err(|_| denied())?;
    request.assertion = Some(signed.assertion);
    Ok(Json(json!({"approved":true})))
}

#[derive(Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
struct SignatureId {
    id: String,
}
async fn signature_poll(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<SignatureId>,
) -> Result<Response> {
    let mut pending = app.pending.lock().await;
    pending.prune();
    let store = app.store.lock().await;
    let (device_id, _) = session_device(&app, &store, &headers)?;
    let key = digest(&body.id);
    let request = pending
        .signatures
        .get(&key)
        .filter(|r| r.device_id == device_id)
        .ok_or_else(|| bad("signature request expired or already used"))?;
    let Some(assertion) = request.assertion.clone() else {
        return Ok((StatusCode::ACCEPTED, Json(json!({"pending":true}))).into_response());
    };
    pending.signatures.remove(&key);
    Ok(Json(json!({"assertion":assertion})).into_response())
}

/// Relay admission: a roster device presenting an access token of a live
/// session bound to it. Heartbeats re-check roster and session liveness.
fn relay_admission(app: App) -> trustd_remote::relay::Authorize {
    Arc::new(move |device, token| {
        let app = app.clone();
        Box::pin(async move {
            let store = app.store.lock().await;
            if !app.roster(&store.saved).devices.contains_key(&device) {
                return Ok(false);
            }
            let now = Utc::now().timestamp();
            let live = |sid: &str| {
                store.saved.sessions.get(sid).is_some_and(|session| {
                    session.device.as_deref() == Some(device.as_str()) && session.expires_at > now
                })
            };
            Ok(match token {
                Some(token) => app
                    .claims(&store, &token)
                    .is_ok_and(|claims| live(&claims.sid)),
                None => store.saved.sessions.keys().any(|sid| live(sid)),
            })
        })
    })
}

fn wire_snapshot(snapshot: &Snapshot, headers: &HeaderMap) -> Value {
    let mut value = json!(snapshot);
    if headers
        .get("x-localshelld-features")
        .and_then(|v| v.to_str().ok())
        != Some("telegram-v1")
    {
        value
            .as_object_mut()
            .expect("snapshot object")
            .remove("telegram");
    }
    value
}
async fn vault_get(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    let store = app.store.lock().await;
    app.authenticate(&store, &headers)?;
    Ok(Json(wire_snapshot(&store.saved.vault, &headers)))
}
async fn vault_put(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Replace>,
) -> Result<Json<Value>> {
    validate_credentials(&body.credentials).map_err(bad)?;
    let mut store = app.store.lock().await;
    app.authenticate(&store, &headers)?;
    if body.expected_revision != store.saved.vault.revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "revision conflict; fetch the current vault and retry",
        ));
    }
    let mut next = store.saved.clone();
    next.vault.revision = next.vault.revision.checked_add(1).ok_or_else(internal)?;
    next.vault.credentials = body.credentials.clone();
    store.commit(next).map_err(|_| internal())?;
    app.revision.send_replace(store.saved.vault.revision);
    Ok(Json(wire_snapshot(&store.saved.vault, &headers)))
}

#[derive(Deserialize)]
struct After {
    after: u64,
}
async fn events(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<After>,
) -> Result<Json<Value>> {
    let mut receiver = app.revision.subscribe();
    let claims = {
        let store = app.store.lock().await;
        app.authenticate(&store, &headers)?
    };
    if *receiver.borrow_and_update() == query.after {
        let seconds = (claims.exp - Utc::now().timestamp()).clamp(1, 25) as u64;
        let _ = tokio::time::timeout(Duration::from_secs(seconds), receiver.changed()).await;
    }
    let store = app.store.lock().await;
    app.authenticate(&store, &headers)?;
    Ok(Json(json!({"revision":store.saved.vault.revision})))
}

async fn assets(uri: axum::http::Uri) -> Response {
    static ASSETS: include_dir::Dir<'_> =
        include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../web/dist");
    let path = if uri.path() == "/" {
        "sync.html"
    } else {
        uri.path().trim_start_matches('/')
    };
    if path != "sync.html" && !path.starts_with("assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(file) = ASSETS.get_file(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [(
            header::CONTENT_TYPE,
            mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string(),
        )],
        file.contents(),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
