//! PowerShell prerequisite installation. Downloads retain the login user's
//! proxy settings; only the MSI process requests elevation.
use super::{App, Job, platform, user_maintenance_lock};
use anyhow::{Context, Result, ensure};
use axum::{Json, extract::State};
use localshelld::server::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Mutex, OnceLock, atomic::Ordering},
    time::{Duration, Instant},
};

const INSTALLER: &str = "https://aka.ms/install-powershell.ps1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallRequest {
    consent: bool,
}

pub(super) async fn install(
    State(state): State<App>,
    Json(request): Json<InstallRequest>,
) -> Result<Json<Value>, ApiError> {
    if !request.consent {
        return Err(ApiError::bad_request("请先同意安装系统级 PowerShell 7。"));
    }
    if state
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(ApiError::conflict("已有安装操作正在执行。"));
    }
    *state.job.lock().unwrap() = Job {
        running: true,
        stage: "powershell".into(),
        ..Job::default()
    };
    tokio::task::spawn_blocking(move || {
        let result = (|| {
            let _lock = user_maintenance_lock(&state.user_config)?;
            install_shell()
        })();
        *state.job.lock().unwrap() = Job {
            stage: if result.is_ok() { "done" } else { "failed" }.into(),
            error: result.as_ref().err().map(|error| format!("{error:#}")),
            completed: result.ok().map(|_| "powershell".into()),
            ..Job::default()
        };
        state.busy.store(false, Ordering::SeqCst);
    });
    Ok(Json(json!({"started": true})))
}

fn permitted(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "aka.ms"
                    | "raw.githubusercontent.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

pub(crate) fn download(url: &str, output: &Path) -> Result<()> {
    let url = reqwest::Url::parse(url)?;
    ensure!(permitted(&url), "unexpected PowerShell download source");
    // Do not clear the environment or opt out of reqwest's automatic proxy
    // discovery: HTTP(S)_PROXY, ALL_PROXY, NO_PROXY and Windows system settings.
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("trustd/", env!("LOCALSHELLD_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 || !permitted(attempt.url()) {
                attempt.error("unexpected PowerShell download redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| anyhow::anyhow!("无法初始化下载连接，请检查代理设置。"))?;
    // Proxy URLs can carry credentials. Never include transport error chains
    // or proxy configuration in installer diagnostics.
    let mut response = client
        .get(url)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|_| anyhow::anyhow!("PowerShell 下载失败，请检查网络和代理设置。"))?;
    let mut file = fs::File::create(output)?;
    response
        .copy_to(&mut file)
        .map_err(|_| anyhow::anyhow!("PowerShell 下载未完成，请重试。"))?;
    file.sync_all()?;
    Ok(())
}

fn install_shell() -> Result<()> {
    super::super::user_service::require_interactive_owner()?;
    let shell = platform::shell()?;
    if shell.is_file() {
        return Ok(());
    }
    let stage = tempfile::Builder::new()
        .prefix("trustd-powershell-")
        .tempdir()?;
    platform::private(stage.path(), true)?;
    let installer = stage.path().join("install-powershell.ps1");
    download(INSTALLER, &installer)?;
    let wrapper = stage.path().join("bootstrap.ps1");
    fs::write(&wrapper, include_bytes!("powershell.ps1"))?;
    let output = super::super::windows_command("WindowsPowerShell/v1.0/powershell.exe")?
        // A wizard launched from pwsh must not pass its PowerShell 7 modules
        // into Windows PowerShell 5.1. Keep proxy environment variables intact.
        .env_remove("PSModulePath")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&wrapper)
        .arg("-InstallerScript")
        .arg(&installer)
        .arg("-Downloader")
        .arg(std::env::current_exe()?)
        .arg("-ExpectedShell")
        .arg(&shell)
        // The upstream script selects its package from this variable. Some
        // application hosts omit it from the inherited environment.
        .env(
            "PROCESSOR_ARCHITECTURE",
            match std::env::consts::ARCH {
                "x86_64" => "AMD64",
                "aarch64" => "ARM64",
                _ => "x86",
            },
        )
        .env("TEMP", stage.path())
        .env("TMP", stage.path())
        .output()?;
    ensure!(
        output.status.success(),
        "PowerShell 安装未完成：{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    ensure!(
        shell.is_file(),
        "安装程序已退出，但仍未找到 {}。",
        shell.display()
    );
    let probe = localshelld::process::background_command(&shell)
        .arg("--version")
        .output()?;
    ensure!(
        probe.status.success(),
        "系统级 PowerShell 无法启动，请修复安装。"
    );
    Ok(())
}

// Status is polled every two seconds. Avoid repeatedly starting Windows
// PowerShell/AppX discovery; refresh soon enough to notice a manual uninstall.
type MsixCache = Mutex<Option<(Instant, Option<Vec<String>>)>>;
pub(super) fn msix_packages() -> Option<Vec<String>> {
    static CACHE: OnceLock<MsixCache> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((when, packages)) = &*cache
        && when.elapsed() < Duration::from_secs(15)
    {
        return packages.clone();
    }
    let result = query_msix().ok();
    *cache = Some((Instant::now(), result.clone()));
    result
}

fn query_msix() -> Result<Vec<String>> {
    let output = super::super::windows_command("WindowsPowerShell/v1.0/powershell.exe")?
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; @(Get-AppxPackage -Name Microsoft.PowerShell; Get-AppxPackage -Name Microsoft.PowerShellPreview) | ForEach-Object { $_.Name }"])
        .output()?;
    ensure!(output.status.success(), "MSIX detection failed");
    let text = String::from_utf8(output.stdout).context("invalid MSIX response")?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|name| {
            matches!(
                *name,
                "Microsoft.PowerShell" | "Microsoft.PowerShellPreview"
            )
        })
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn downloads_require_official_https_origins() {
        for url in [
            INSTALLER,
            "https://raw.githubusercontent.com/PowerShell/PowerShell/master/tools/metadata.json",
            "https://github.com/PowerShell/PowerShell/releases/download/v99.0/PowerShell-99.0-win-x64.msi",
        ] {
            assert!(permitted(&url.parse().unwrap()));
        }
        for url in [
            "http://github.com/file",
            "https://github.com.evil.example/file",
            "https://user:password@github.com/file",
        ] {
            assert!(!permitted(&url.parse().unwrap()));
        }
    }
}
