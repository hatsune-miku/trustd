use super::{Peer, Saved};
use anyhow::{Result, ensure};
use clap::Subcommand;
use serde_json::json;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum SetupCommand {
    /// Print this device's public identity. No private key or access token is printed.
    Identity {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Save relay configuration while the user daemon is stopped.
    Connect {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        relay: String,
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Pair a public device key and explicitly grant its requested access.
    Pair {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        public_key: String,
        #[arg(long)]
        label: String,
        #[arg(long)]
        allow_execution: bool,
        #[arg(long)]
        sudo: bool,
        #[arg(long = "secret-key")]
        secret_keys: Vec<String>,
    },
}

pub fn configure_local(command: SetupCommand) -> Result<()> {
    let config = match &command {
        SetupCommand::Identity { config }
        | SetupCommand::Connect { config, .. }
        | SetupCommand::Pair { config, .. } => config
            .clone()
            .map_or_else(crate::config::user_config_path, Ok)?,
    };
    ensure!(
        config.is_file(),
        "initialize the user daemon configuration first"
    );
    let path = config.with_extension("remote.json");
    let mut saved: Saved = if path.exists() {
        serde_json::from_slice(&std::fs::read(&path)?)?
    } else {
        Saved {
            identity: trustd_remote::Identity::generate()?,
            relay_url: String::new(),
            relay_token: String::new(),
            peers: Default::default(),
        }
    };
    match command {
        SetupCommand::Identity { .. } => {}
        SetupCommand::Connect {
            relay, token_file, ..
        } => {
            let url = reqwest::Url::parse(&relay)?;
            ensure!(
                matches!(url.scheme(), "https" | "wss")
                    || url.host_str().is_some_and(|h| h == "localhost"
                        || h.parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())),
                "relay must use HTTPS/WSS"
            );
            saved.relay_url = relay.trim_end_matches('/').into();
            saved.relay_token = std::fs::read_to_string(token_file)?.trim().into();
            ensure!(!saved.relay_token.is_empty(), "relay token file is empty");
        }
        SetupCommand::Pair {
            public_key,
            label,
            allow_execution,
            sudo,
            secret_keys,
            ..
        } => {
            let id = trustd_remote::node_id(&public_key)?;
            ensure!(
                id != saved.identity.id()?,
                "cannot pair with this device itself"
            );
            ensure!(
                !label.trim().is_empty() && label.len() <= 128,
                "invalid device label"
            );
            saved.peers.insert(
                id,
                Peer {
                    label,
                    public_key,
                    allow_execution,
                    sudo,
                    secret_keys: crate::secrets::normalize_keys(&secret_keys)?,
                },
            );
        }
    }
    crate::storage::save_json(&path, &saved)?;
    println!(
        "{}",
        json!({"device_id":saved.identity.id()?,"public_key":saved.identity.public_key,"relay_url":saved.relay_url,"restart_required":true})
    );
    Ok(())
}
