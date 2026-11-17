//! Outbound-only relay transport. The relay never receives Noise private keys.
#![forbid(unsafe_code)]

pub mod relay;
mod transport;

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub use transport::{Handler, Node, TokenSource, static_token};

const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
pub const VERSION: &str = env!("TRUSTD_REMOTE_VERSION");
const MAX_FRAME: usize = 96 * 1024;
const MAX_PAYLOAD: usize = 2 * 1024 * 1024;
const CHUNK: usize = 48 * 1024;

#[derive(Clone, Serialize, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct Identity {
    pub private_key: String,
    pub public_key: String,
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let key = snow::Builder::new(PATTERN.parse()?).generate_keypair()?;
        Ok(Self {
            private_key: STANDARD.encode(key.private),
            public_key: STANDARD.encode(key.public),
        })
    }

    pub fn id(&self) -> Result<String> {
        node_id(&self.public_key)
    }
}

pub fn node_id(public_key: &str) -> Result<String> {
    let bytes = STANDARD.decode(public_key)?;
    ensure!(bytes.len() == 32, "invalid device public key");
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Frame {
    #[serde(default)]
    pub from: String,
    pub to: String,
    pub channel: String,
    pub kind: String,
    #[serde(default)]
    pub data: String,
}

impl Frame {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.to.len() == 64 && self.to.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid target"
        );
        ensure!(
            uuid::Uuid::parse_str(&self.channel).is_ok(),
            "invalid channel"
        );
        ensure!(
            matches!(self.kind.as_str(), "open" | "data" | "close" | "offline"),
            "invalid frame kind"
        );
        ensure!(self.data.len() <= MAX_FRAME - 512, "frame too large");
        Ok(())
    }
}
