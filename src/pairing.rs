//! Installer-provisioned trust between the user and privileged daemons of one
//! machine. Installation already runs with Administrator/root approval, which
//! proves the same owner controls both daemons, so no pairing code is needed.
//!
//! The installer never edits a daemon's authorization file. It stages the key
//! beside each configuration; the daemon adopts it at its next start.
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};

pub use crate::settings::Pairing;
use crate::{settings::SavedSettings, storage};

/// Staged pairing for the daemon whose configuration is `config`.
pub fn provision_path(config: &Path) -> PathBuf {
    config.with_extension("pairing.json")
}

fn authorization_path(config: &Path) -> PathBuf {
    config.with_extension("authorization.json")
}

fn valid(pairing: &Pairing) -> bool {
    pairing.peer.ip().is_loopback()
        && pairing.peer.port() != 0
        && crate::bridge::validate_key(&pairing.key).is_ok()
}

fn staged(config: &Path) -> Result<Option<Pairing>> {
    match fs::read(provision_path(config)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Stage `pairing` for the daemon configured at `config` to adopt when it starts.
pub fn provision(config: &Path, pairing: &Pairing) -> Result<()> {
    ensure!(
        valid(pairing),
        "pairing requires a valid key and a loopback peer"
    );
    storage::save_json(&provision_path(config), pairing)
}

/// User half of installation pairing. Keeps the user daemon's key when it
/// already points at `privileged`, so reinstalling never breaks working trust;
/// otherwise stages a new key. Returns the key the privileged daemon must adopt.
pub fn ensure_user(config: &Path, privileged: SocketAddr) -> Result<String> {
    let usable = |pairing: &Pairing| pairing.peer == privileged && valid(pairing);
    if let Some(pairing) = staged(config)?.filter(|pairing| usable(pairing)) {
        return Ok(pairing.key);
    }
    let saved: SavedSettings = storage::load_json(&authorization_path(config))?;
    if let Some(pairing) = saved.pairing.filter(|pairing| usable(pairing)) {
        // A stale staged file would otherwise replace the working key at startup.
        match fs::remove_file(provision_path(config)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error.into());
            }
            _ => {}
        }
        return Ok(pairing.key);
    }
    let pairing = Pairing {
        key: crate::bridge::generate_key(),
        peer: privileged,
    };
    provision(config, &pairing)?;
    Ok(pairing.key)
}

/// Adopt a staged pairing into `saved` and persist it. Called once at daemon
/// startup, before any request can observe the previous pairing.
pub(crate) fn adopt(config: &Path, saved: &mut SavedSettings) -> Result<()> {
    let Some(pairing) = staged(config)? else {
        return Ok(());
    };
    ensure!(
        valid(&pairing),
        "staged pairing {} is invalid; rerun installation to repair it",
        provision_path(config).display()
    );
    saved.pairing = Some(pairing);
    storage::save_json(&authorization_path(config), saved)?;
    fs::remove_file(provision_path(config))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing(peer: &str) -> Pairing {
        Pairing {
            key: crate::bridge::generate_key(),
            peer: peer.parse().unwrap(),
        }
    }

    #[test]
    fn user_half_reuses_working_trust_and_stages_only_when_needed() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let privileged: SocketAddr = "127.0.0.1:32119".parse().unwrap();
        // Fresh install: a new key is staged and returned.
        let first = ensure_user(&config, privileged).unwrap();
        assert_eq!(staged(&config).unwrap().unwrap().key, first);
        // Repeated before the daemon started: the same staged key is kept.
        assert_eq!(ensure_user(&config, privileged).unwrap(), first);
        // After adoption, reinstalling keeps the working key and stages nothing.
        let mut saved = SavedSettings::default();
        adopt(&config, &mut saved).unwrap();
        assert!(!provision_path(&config).exists());
        assert_eq!(saved.pairing.as_ref().unwrap().key, first);
        assert_eq!(ensure_user(&config, privileged).unwrap(), first);
        assert!(!provision_path(&config).exists());
        // A different privileged endpoint gets a new key instead of reusing trust.
        let other = ensure_user(&config, "127.0.0.1:42119".parse().unwrap()).unwrap();
        assert_ne!(other, first);
        // A stale staged file never overrides the working key.
        let mut stale = pairing("127.0.0.1:1");
        stale.key = "not-a-key".into();
        storage::save_json(&provision_path(&config), &stale).unwrap();
        assert_eq!(ensure_user(&config, privileged).unwrap(), first);
        assert!(!provision_path(&config).exists());
    }

    #[test]
    fn adoption_replaces_trust_once_and_rejects_invalid_staging() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut saved = SavedSettings {
            pairing: Some(pairing("127.0.0.1:32120")),
            ..SavedSettings::default()
        };
        let next = pairing("127.0.0.1:32121");
        provision(&config, &next).unwrap();
        adopt(&config, &mut saved).unwrap();
        assert_eq!(saved.pairing.as_ref().unwrap().key, next.key);
        let persisted: SavedSettings = storage::load_json(&authorization_path(&config)).unwrap();
        assert_eq!(persisted.pairing.unwrap().peer, next.peer);
        assert!(!provision_path(&config).exists());
        assert!(provision(&config, &pairing("10.0.0.1:32120")).is_err());
        storage::save_json(
            &provision_path(&config),
            &Pairing {
                key: "short".into(),
                peer: "127.0.0.1:32120".parse().unwrap(),
            },
        )
        .unwrap();
        assert!(adopt(&config, &mut saved).is_err());
        assert_eq!(saved.pairing.unwrap().key, next.key);
    }
}
