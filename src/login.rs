//! Emergency management login for the daemon's own operating-system user.
//!
//! `trustd login-url` writes a single-use ticket beside the configuration and
//! prints a login link. Any process of that user can already read the daemon's
//! configuration and keys, so this adds no access it did not have; it lets the
//! owner manage the device while the backend passkey login is unavailable.
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage;

const TICKET_SECONDS: i64 = 300;

#[derive(Serialize, Deserialize)]
struct Ticket {
    hash: String,
    expires_at: i64,
}

fn path(config: &Path) -> PathBuf {
    config.with_extension("login-ticket")
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// Create a ticket for the daemon configured at `config`; returns the secret.
pub fn write_ticket(config: &Path) -> Result<String> {
    let ticket = crate::server::strong_handle();
    storage::save_json(
        &path(config),
        &Ticket {
            hash: digest(&ticket),
            expires_at: chrono::Utc::now().timestamp() + TICKET_SECONDS,
        },
    )?;
    Ok(ticket)
}

/// Consume the ticket. Any attempt removes it, so a ticket is never guessed.
pub(crate) fn consume_ticket(config: &Path, ticket: &str) -> bool {
    let path = path(config);
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice::<Ticket>(&bytes).is_ok_and(|saved| {
        saved.expires_at > chrono::Utc::now().timestamp() && saved.hash == digest(ticket)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_are_single_use_expire_and_are_removed_by_any_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        assert!(!consume_ticket(&config, "missing"));
        let ticket = write_ticket(&config).unwrap();
        assert!(
            !std::fs::read_to_string(path(&config))
                .unwrap()
                .contains(&ticket)
        );
        assert!(consume_ticket(&config, &ticket));
        assert!(!consume_ticket(&config, &ticket));
        // A wrong guess burns the ticket.
        let ticket = write_ticket(&config).unwrap();
        assert!(!consume_ticket(&config, "guess"));
        assert!(!consume_ticket(&config, &ticket));
        storage::save_json(
            &path(&config),
            &Ticket {
                hash: digest("old"),
                expires_at: 1,
            },
        )
        .unwrap();
        assert!(!consume_ticket(&config, "old"));
    }
}
