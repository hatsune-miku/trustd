//! Ephemeral, login-scoped setup UI. The privileged helper accepts a digest-bound
//! plan with a closed set of operations, not commands or installation paths.
mod agents;
mod apply;
mod platform;
#[cfg(windows)]
pub(crate) mod powershell;
mod programs;
#[cfg(any(target_os = "linux", test))]
pub(crate) mod terminal;

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use localshelld::{
    assets,
    config::{Config, user_config_path},
    paths::MachinePaths,
    server::ApiError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub(crate) use apply::apply;
pub(crate) use programs::relocate;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Install,
    Uninstall,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    protocol: u32,
    operation: Operation,
    binary_hash: String,
    binaries: super::bundle::Manifest,
    password_hash: Option<String>,
    owner: String,
    user_config: PathBuf,
    system_bind: std::net::SocketAddr,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    password: Option<String>,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    replace_agents: bool,
}

#[derive(Clone, Default, Serialize)]
struct Job {
    running: bool,
    stage: String,
    error: Option<String>,
    completed: Option<String>,
    agents: Vec<agents::Outcome>,
}

struct Inner {
    user_headless: bool,
    token: String,
    authority: String,
    paths: MachinePaths,
    executable: PathBuf,
    binary_hash: String,
    binaries: super::bundle::Manifest,
    user_config: PathBuf,
    owner: String,
    agents: Vec<agents::Agent>,
    busy: AtomicBool,
    job: Mutex<Job>,
    shutdown: tokio::sync::Notify,
}
type App = Arc<Inner>;

pub async fn run(no_open: bool, mode: Option<bool>) -> Result<()> {
    #[cfg(windows)]
    super::user_service::require_interactive_owner()?;
    #[cfg(not(windows))]
    super::require_user().context(
        "start maintenance as the logged-in user; the wizard requests elevation when needed",
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let authority = listener.local_addr()?.to_string();
    let state = tokio::task::spawn_blocking(move || prepare(authority, mode)).await??;
    let url = format!(
        "http://{}/maintenance.html#{}",
        state.authority, state.token
    );
    // Fragment is never sent in HTTP requests or referrers. No persistent token
    // file, cookie, localStorage or daemon endpoint is created by maintenance.
    println!("Installation and maintenance: {url}");
    if !no_open {
        let open_url = url.clone();
        if let Err(error) =
            tokio::task::spawn_blocking(move || platform::open_browser(&open_url)).await?
        {
            eprintln!("Could not open the browser: {error}. Open the local URL above.");
        }
    }
    let stop = state.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            tokio::select! { _ = super::shutdown_signal() => {}, _ = stop.shutdown.notified() => {}, _ = tokio::time::sleep(Duration::from_secs(30 * 60)) => {} }
        }).await?;
    Ok(())
}

fn prepare(authority: String, mode: Option<bool>) -> Result<App> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let binaries =
        super::bundle::Manifest::read(executable.parent().context("missing bundle directory")?)?;
    let user_config = user_config_path()?;
    let saved = if user_config.try_exists()? {
        Config::load(&user_config)?.user_headless
    } else {
        None
    };
    let user_headless = super::user_service::resolve_headless(mode, saved);
    let binary_hash = binaries.digest()?;
    Ok(Arc::new(Inner {
        user_headless,
        token: format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
        authority,
        paths: MachinePaths::resolve()?,
        executable,
        binary_hash,
        binaries,
        user_config,
        owner: platform::owner()?,
        agents: agents::catalog()?,
        busy: AtomicBool::new(false),
        job: Mutex::new(Job::default()),
        shutdown: tokio::sync::Notify::new(),
    }))
}

fn router(state: App) -> Router {
    let router = Router::new();
    #[cfg(windows)]
    let router = router.route("/setup/v1/powershell", post(powershell::install));
    router
        .route("/setup/v1/status", get(status))
        .route("/setup/v1/install", post(install))
        .route("/setup/v1/uninstall", post(uninstall))
        .route("/setup/v1/close", post(close))
        .fallback(get(static_asset))
        .layer(DefaultBodyLimit::max(8192))
        .layer(middleware::from_fn_with_state(state.clone(), security))
        .with_state(state)
}

async fn security(State(state): State<App>, request: Request, next: Next) -> Response {
    let host = request.headers().get("host").and_then(|h| h.to_str().ok());
    let expected_origin = format!("http://{}", state.authority);
    let origin_ok = request
        .headers()
        .get("origin")
        .is_none_or(|h| h.to_str().ok() == Some(&expected_origin));
    let site_ok = request
        .headers()
        .get("sec-fetch-site")
        .is_none_or(|h| matches!(h.to_str(), Ok("same-origin" | "none")));
    if host != Some(state.authority.as_str()) || !origin_ok || !site_ok {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.uri().path().starts_with("/setup/") {
        let expected = format!("Bearer {}", state.token);
        if request
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            != Some(&expected)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("content-security-policy", assets::CSP),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

async fn static_asset(uri: axum::http::Uri) -> Response {
    if uri.path() != "/maintenance.html" && !uri.path().starts_with("/assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    assets::serve(uri).await
}

fn snapshot(state: &Inner) -> Result<Value> {
    let registration = super::update::service::registration()?;
    // A non-elevated wizard may be unable to inspect protected system state.
    // In that case it can still collect a password for a missing configuration.
    let system_initialized = apply::configuration_source(
        &state.paths,
        registration.as_ref().map(|value| value.config.as_path()),
    )
    .map(|source| source.is_some())
    .ok();
    let user = if state.user_config.try_exists()? {
        Some(Config::load(&state.user_config)?)
    } else {
        None
    };
    let shell = platform::shell()?;
    let job = state.job.lock().unwrap().clone();
    Ok(json!({
        "version": localshelld::VERSION, "platform": std::env::consts::OS,
        "program": state.paths.program, "system_config": state.paths.config,
        "user_config": state.user_config, "registered": registration.is_some(),
        "user_headless": state.user_headless,
        "registered_executable": registration.as_ref().map(|value| &value.executable),
        "registered_config": registration.as_ref().map(|value| &value.config),
        "system_running": registration.as_ref().is_some_and(|value| value.was_running),
        "system_initialized": system_initialized,
        "powershell_msix": powershell_msix(),
        "user_initialized": user.is_some(), "shell": shell, "shell_available": shell.is_file(),
        "elevation_available": platform::elevation_available(),
        "user_url": format!("http://localhost:{}/", user.as_ref().map_or(localshelld::config::USER_PORT, |value| value.bind.port())),
        "system_url": format!("http://localhost:{}/", user.as_ref().map_or(localshelld::config::DEFAULT_PORT, |value| value.privileged_daemon.port())),
        "job": job,
        "agents": state.agents,
    }))
}

fn powershell_msix() -> Option<Vec<String>> {
    #[cfg(windows)]
    {
        powershell::msix_packages()
    }
    #[cfg(not(windows))]
    {
        Some(vec![])
    }
}

async fn status(State(state): State<App>) -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(move || snapshot(&state))
        .await
        .map_err(|_| ApiError::internal("installation status worker stopped"))?
        .map(Json)
        .map_err(|error| ApiError::internal(format!("{error:#}")))
}

fn start_job(
    state: App,
    operation: Operation,
    password: Zeroizing<Option<String>>,
    selected: Vec<String>,
    replace_agents: bool,
) -> Result<Json<Value>, ApiError> {
    if state
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(ApiError::conflict("已有安装操作正在执行。"));
    }
    *state.job.lock().unwrap() = Job {
        running: true,
        stage: "preparing".into(),
        ..Job::default()
    };
    tokio::task::spawn_blocking(move || {
        let elevation = platform::Elevation::Graphical;
        #[cfg(target_os = "macos")]
        let elevation = if state.user_headless {
            platform::Elevation::Terminal
        } else {
            elevation
        };
        let result = execute(
            &state,
            operation,
            password,
            &selected,
            replace_agents,
            elevation,
        );
        *state.job.lock().unwrap() = Job {
            running: false,
            stage: if result.is_ok() { "done" } else { "failed" }.into(),
            error: result.as_ref().err().map(|error| format!("{error:#}")),
            agents: result.as_ref().ok().cloned().unwrap_or_default(),
            completed: result.ok().map(|_| {
                if operation == Operation::Install {
                    "install"
                } else {
                    "uninstall"
                }
                .into()
            }),
        };
        state.busy.store(false, Ordering::SeqCst);
    });
    Ok(Json(json!({"started":true})))
}

async fn install(
    State(state): State<App>,
    Json(mut body): Json<InstallRequest>,
) -> Result<Json<Value>, ApiError> {
    agents::validate(&body.agents, &state.agents)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    if body
        .password
        .as_ref()
        .is_some_and(|value| value.len() > 4096)
    {
        return Err(ApiError::bad_request("password is too long"));
    }
    start_job(
        state,
        Operation::Install,
        Zeroizing::new(body.password.take()),
        std::mem::take(&mut body.agents),
        body.replace_agents,
    )
}
async fn uninstall(
    State(state): State<App>,
    Json(_): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, ApiError> {
    start_job(
        state,
        Operation::Uninstall,
        Zeroizing::new(None),
        vec![],
        false,
    )
}
async fn close(
    State(state): State<App>,
    Json(_): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, ApiError> {
    if state.busy.load(Ordering::SeqCst) {
        return Err(ApiError::conflict("安装操作尚未结束。"));
    }
    state.shutdown.notify_one();
    Ok(Json(json!({"closed":true})))
}

fn execute(
    state: &Inner,
    operation: Operation,
    mut password: Zeroizing<Option<String>>,
    selected: &[String],
    replace_agents: bool,
    elevation: platform::Elevation,
) -> Result<Vec<agents::Outcome>> {
    elevation.check()?;
    // Hold across privileged apply AND user registration/configuration. The
    // machine helper also locks its own state; neither lock file is unlinked.
    let _lock = user_maintenance_lock(&state.user_config)?;
    if operation == Operation::Install {
        // Hash every role, but never load the desktop executable on a headless host.
        for name in state.binaries.0.keys() {
            if state.user_headless && *name == super::bundle::filename(super::bundle::USER) {
                continue;
            }
            let program = state.executable.parent().unwrap().join(name);
            let version = super::update::probe(&program, name.trim_end_matches(".exe"))
                .with_context(|| {
                    format!("{name} 无法启动，尚未修改配置或服务；无图形环境请使用 --headless。")
                })?;
            ensure!(version == localshelld::VERSION, "套件中的程序版本不一致。");
        }
    }
    let shell = platform::shell()?;
    ensure!(
        operation == Operation::Uninstall || shell.is_file(),
        "请先安装系统级 PowerShell 7，再刷新环境检查。"
    );
    // Some("") is an explicitly chosen empty password; None means preserve.
    let password_hash = password
        .as_ref()
        .map(|value| localshelld::auth::hash_password(value.as_bytes()))
        .transpose()?;
    password.zeroize();
    if operation == Operation::Install && !state.user_config.try_exists()? {
        let hash = password_hash.clone().context("请设置初始化主密码。")?;
        let config = Config {
            password_hash: hash,
            bind: format!("127.0.0.1:{}", localshelld::config::USER_PORT).parse()?,
            shell: shell.to_string_lossy().into_owned(),
            user_headless: Some(state.user_headless),
            ..Config::default()
        };
        config.save(&state.user_config)?;
        localshelld::config::load_or_create_seal_key(&localshelld::config::seal_key_path(
            &state.user_config,
        ))?;
    }
    let user = if state.user_config.try_exists()? {
        let mut config = Config::load(&state.user_config)?;
        if operation == Operation::Install {
            config.user_headless = Some(state.user_headless);
            #[cfg(windows)]
            {
                config.shell = shell.to_string_lossy().into_owned();
            }
            config.save(&state.user_config)?;
        }
        Some(config)
    } else {
        None
    };
    let plan = Plan {
        protocol: 2,
        operation,
        binary_hash: state.binary_hash.clone(),
        binaries: state.binaries.clone(),
        password_hash,
        owner: state.owner.clone(),
        user_config: state.user_config.clone(),
        system_bind: user.as_ref().map_or(
            format!("127.0.0.1:{}", localshelld::config::DEFAULT_PORT).parse()?,
            |value| value.privileged_daemon,
        ),
    };
    let stage = tempfile::Builder::new()
        .prefix("trustd-maintenance-")
        .tempdir()?;
    platform::private(stage.path(), true)?;
    let helper = stage
        .path()
        .join(super::bundle::filename(super::bundle::MAINTENANCE));
    state.binaries.copy(
        state
            .executable
            .parent()
            .context("missing bundle directory")?,
        stage.path(),
    )?;
    let bytes = Zeroizing::new(serde_json::to_vec(&plan)?);
    let path = stage.path().join("plan.json");
    std::fs::write(&path, &*bytes)?;
    platform::private(&path, false)?;
    let digest = format!("{:x}", Sha256::digest(&*bytes));
    #[cfg(unix)]
    super::user_service::uninstall()?;
    set_stage(state, "elevation");
    elevation.apply(&helper, &path, &digest).with_context(|| {
        format!(
            "安装未完成或系统授权已取消。已创建的配置会保留；诊断文件：{}",
            state.paths.data.join("maintenance-error.txt").display()
        )
    })?;
    if operation == Operation::Install {
        set_stage(state, "user_service");
        let executable = state.paths.executable();
        let user = Config::load(&state.user_config)?;
        let user_executable = super::bundle::beside(&executable, super::user_service::role(&user))?;
        #[cfg(not(windows))]
        {
            super::installation::wait_listener_closed(
                Config::load(&state.user_config)?.bind,
                Duration::from_secs(30),
            )?;
            super::user_service::install_at(&user_executable, &state.user_config)?;
        }
        let user = Config::load(&state.user_config)?;
        super::installation::wait_healthy_version(
            user.privileged_daemon,
            "privileged",
            Some(localshelld::VERSION),
            Duration::from_secs(30),
        )?;
        super::installation::wait_healthy_version(
            user.bind,
            "user",
            Some(localshelld::VERSION),
            Duration::from_secs(30),
        )?;
        super::user_service::verify_running_at(&user_executable, &state.user_config)?;
        if matches!(elevation, platform::Elevation::Graphical) {
            platform::shortcut(&executable)?;
        }
        set_stage(state, "agents");
        let stdio = super::bundle::beside(&executable, "trustd-stdio")?;
        return Ok(agents::configure(
            &state.agents,
            selected,
            &stdio,
            user.bind,
            replace_agents,
        ));
    }
    Ok(vec![])
}

fn user_maintenance_lock(config: &Path) -> Result<std::fs::File> {
    let directory = config
        .parent()
        .context("missing user configuration directory")?;
    std::fs::create_dir_all(directory)?;
    let path = directory.join("maintenance.lock");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "invalid maintenance lock"
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    metadata.file_attributes() & 0x400 == 0,
                    "invalid maintenance lock"
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.uid() == rustix::process::getuid().as_raw() && metadata.nlink() == 1,
                    "invalid maintenance lock owner/links"
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    platform::private(&path, false)?;
    fs2::FileExt::try_lock_exclusive(&file).context("另一个安装或卸载任务正在运行。")?;
    Ok(file)
}

fn set_stage(state: &Inner, stage: &str) {
    state.job.lock().unwrap().stage = stage.into();
}

fn digest_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests;
