use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand};
use rand_core::{OsRng, RngCore};
use std::{net::SocketAddr, path::PathBuf};
use trustd_remote::relay::{self, Config};

#[derive(Parser)]
#[command(name="trustd-relay",version=trustd_remote::VERSION, about = "Authenticated, ciphertext-only trustd relay")]
struct Options {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "127.0.0.1:32129")]
        bind: SocketAddr,
    },
    AddDevice {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        device: String,
        #[arg(long)]
        token_file: PathBuf,
    },
    RemoveDevice {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        device: String,
    },
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
        use std::os::unix::fs::PermissionsExt;
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
    match Options::parse().command {
        Command::Serve { config, bind } => {
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
        Command::AddDevice {
            config,
            device,
            token_file,
        } => {
            ensure!(
                device.len() == 64 && device.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid device ID"
            );
            let mut data = if config.exists() {
                serde_json::from_slice::<Config>(&std::fs::read(&config)?)?
            } else {
                Config::default()
            };
            let mut token = [0; 32];
            OsRng.fill_bytes(&mut token);
            let token = URL_SAFE_NO_PAD.encode(token);
            write_private(&token_file, token.as_bytes())?;
            data.devices.insert(device, relay::token_hash(&token));
            write_private(&config, &serde_json::to_vec_pretty(&data)?)?;
            println!(
                "Device registered; access token written to {}",
                token_file.display()
            );
        }
        Command::RemoveDevice { config, device } => {
            let mut data: Config = serde_json::from_slice(&std::fs::read(&config)?)?;
            data.devices.remove(&device);
            write_private(&config, &serde_json::to_vec_pretty(&data)?)?;
            println!("Device removed");
        }
    }
    Ok(())
}
