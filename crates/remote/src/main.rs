use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Args, Parser, Subcommand};
use rand_core::{OsRng, RngCore};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};
use trustd_remote::relay::{self, Config};

#[derive(Parser)]
#[command(name="trustd-relay",version=trustd_remote::VERSION, about = "Authenticated, ciphertext-only trustd relay")]
struct Options {
    /// Relay registry shared by serve, add-device and remove-device.
    #[arg(long, global = true, value_name = "PATH", default_value_os_t = default_config_path())]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the relay behind an HTTPS reverse proxy.
    Serve {
        #[arg(long, default_value = "127.0.0.1:32129")]
        bind: SocketAddr,
    },
    /// Register a device or replace its relay access token.
    AddDevice {
        #[command(flatten)]
        device: Device,
        /// Write the generated token here. Defaults to <config directory>/<device ID>.token.
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
    },
    /// Revoke a device's relay access.
    RemoveDevice {
        #[command(flatten)]
        device: Device,
    },
}

#[derive(Args)]
struct Device {
    /// Device ID or base64 public key copied from the device's Remote page.
    #[arg(value_name = "DEVICE", value_parser = device_id,
        required_unless_present = "legacy_device", conflicts_with = "legacy_device")]
    value: Option<String>,
    #[arg(long = "device", hide = true, value_parser = device_id)]
    legacy_device: Option<String>,
}

impl Device {
    fn id(self) -> String {
        self.value
            .or(self.legacy_device)
            .expect("device is required")
    }
}

fn device_id(value: &str) -> std::result::Result<String, String> {
    let value = value.trim();
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(value.to_ascii_lowercase());
    }
    trustd_remote::node_id(value)
        .map_err(|_| "expected a 64-character device ID or a base64 device public key".into())
}

fn default_config_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into()))
            .join("trustd-relay")
            .join("config.json")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/trustd-relay/config.json")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        PathBuf::from("/var/lib/trustd-relay/config.json")
    }
}

fn default_token_file(config: &Path, device: &str) -> PathBuf {
    config
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{device}.token"))
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
        // An administrator may update a registry owned by the service account.
        // Atomic replacement must not make that registry unreadable to the relay.
        match std::fs::metadata(path) {
            Ok(previous) => chown(file.path(), Some(previous.uid()), Some(previous.gid()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    localshelld_native_process::restrict_path(
        file.path(),
        false,
        localshelld_native_process::FileAccess::Private,
    )?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("trustd_remote=info")
        .init();
    let options = Options::parse();
    let config = options.config;
    match options.command {
        Command::Serve { bind } => {
            ensure!(
                bind.ip().is_loopback(),
                "bind the relay behind an HTTPS reverse proxy on loopback"
            );
            let _: Config = serde_json::from_slice(&std::fs::read(&config)?)?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(%bind, "relay ready");
            axum::serve(listener, relay::router(config))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Command::AddDevice { device, token_file } => {
            let device = device.id();
            let token_file = token_file.unwrap_or_else(|| default_token_file(&config, &device));
            let mut data = if config.exists() {
                serde_json::from_slice::<Config>(&std::fs::read(&config)?)?
            } else {
                Config::default()
            };
            let mut token = [0; 32];
            OsRng.fill_bytes(&mut token);
            let token = URL_SAFE_NO_PAD.encode(token);
            write_private(&token_file, token.as_bytes())?;
            data.devices
                .insert(device.clone(), relay::token_hash(&token));
            write_private(&config, &serde_json::to_vec_pretty(&data)?)?;
            println!(
                "Device {device} registered; access token written to {}",
                token_file.display()
            );
        }
        Command::RemoveDevice { device } => {
            let device = device.id();
            let mut data: Config = serde_json::from_slice(&std::fs::read(&config)?)?;
            data.devices.remove(&device);
            write_private(&config, &serde_json::to_vec_pretty(&data)?)?;
            println!("Device removed");
        }
    }
    Ok(())
}
