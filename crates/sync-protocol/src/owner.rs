//! Owner-signed statements verified by every device, not only by the backend.
//!
//! The owner's passkey signs the exact statement text: the WebAuthn challenge
//! is a hash of it, so any device holding the owner's public keys can check an
//! approval without trusting the backend that stored or delivered it.
use std::collections::{BTreeMap, BTreeSet};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use webauthn_rs::prelude::{Base64UrlSafeData, COSEKey, Passkey, PublicKeyCredential};

const CHALLENGE_DOMAIN: &[u8] = b"trustd-owner-statement-v1\n";
const MAX_STATEMENT: usize = 16 * 1024;
const USER_PRESENT: u8 = 0x01;
const USER_VERIFIED: u8 = 0x04;

/// One owner passkey as devices pin it: its credential ID and public key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerKey {
    pub credential_id: Base64UrlSafeData,
    pub public_key: COSEKey,
}

impl From<&Passkey> for OwnerKey {
    fn from(passkey: &Passkey) -> Self {
        Self {
            credential_id: passkey.cred_id().as_ref().to_vec().into(),
            public_key: passkey.get_public_key().clone(),
        }
    }
}

/// The relying party and keys a device pins when it joins.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    pub rp_id: String,
    pub origin: String,
    pub keys: Vec<OwnerKey>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Statement {
    /// Admit a device's Noise static public key to the owner's devices.
    DeviceApprove {
        device_id: String,
        public_key: String,
        name: String,
        issued_at: i64,
    },
    /// Permanently remove a device; its ID can never be approved again.
    DeviceRevoke { device_id: String, issued_at: i64 },
    /// Trust another owner passkey (signed by an already trusted one).
    PasskeyAdd { key: OwnerKey, issued_at: i64 },
    /// Stop trusting an owner passkey.
    PasskeyRemove {
        credential_id: Base64UrlSafeData,
        issued_at: i64,
    },
    /// One-time login to a device's management page. Never stored in the
    /// roster; the device checks its own nonce and the statement's age.
    ManagementLogin {
        device_id: String,
        nonce: String,
        issued_at: i64,
    },
}

/// Statement text exactly as signed, with the owner's WebAuthn assertion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedStatement {
    pub statement: String,
    pub assertion: PublicKeyCredential,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

/// Device ID: lowercase hex SHA-256 of the 32-byte Noise public key.
pub fn device_id(public_key: &str) -> Result<String> {
    let bytes = STANDARD
        .decode(public_key)
        .map_err(|_| Error("invalid device public key"))?;
    if bytes.len() != 32 {
        return Err(Error("invalid device public key"));
    }
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn valid_device_id(id: &str) -> Result<()> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error("invalid device ID"));
    }
    Ok(())
}

/// The WebAuthn challenge that signs `statement`.
pub fn challenge(statement: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(CHALLENGE_DOMAIN);
    hash.update(statement.as_bytes());
    hash.finalize().into()
}

impl Statement {
    /// Parse and validate statement text. Unknown fields are rejected so the
    /// signed text cannot carry meaning a verifier ignores.
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > MAX_STATEMENT {
            return Err(Error("statement is too large"));
        }
        let statement: Self = serde_json::from_str(text).map_err(|_| Error("invalid statement"))?;
        match &statement {
            Self::DeviceApprove {
                device_id: id,
                public_key,
                name,
                ..
            } => {
                if device_id(public_key)? != *id {
                    return Err(Error("device ID does not match its public key"));
                }
                if name.trim().is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
                    return Err(Error("device name must contain 1–80 printable bytes"));
                }
            }
            Self::DeviceRevoke { device_id: id, .. } => valid_device_id(id)?,
            Self::ManagementLogin {
                device_id: id,
                nonce,
                ..
            } => {
                valid_device_id(id)?;
                if nonce.len() < 16 || nonce.len() > 128 {
                    return Err(Error("invalid login nonce"));
                }
            }
            Self::PasskeyAdd { .. } | Self::PasskeyRemove { .. } => {}
        }
        Ok(statement)
    }

    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("statement serialization")
    }
}

/// Verify a WebAuthn assertion over `challenge` by one of `keys`, for the
/// owner's relying party and origin, with user presence and verification.
pub fn verify_assertion(
    owner: &Owner,
    keys: &[OwnerKey],
    challenge: &[u8],
    credential: &PublicKeyCredential,
) -> Result<()> {
    let key = keys
        .iter()
        .find(|key| key.credential_id.as_ref() == credential.raw_id.as_ref())
        .ok_or(Error("assertion is not from a trusted owner passkey"))?;
    let client_data: &[u8] = credential.response.client_data_json.as_ref();
    #[derive(Deserialize)]
    struct ClientData {
        #[serde(rename = "type")]
        kind: String,
        challenge: String,
        origin: String,
        #[serde(rename = "crossOrigin", default)]
        cross_origin: bool,
    }
    let parsed: ClientData =
        serde_json::from_slice(client_data).map_err(|_| Error("invalid client data"))?;
    if parsed.kind != "webauthn.get"
        || parsed.cross_origin
        || parsed.origin != owner.origin
        || parsed.challenge != URL_SAFE_NO_PAD.encode(challenge)
    {
        return Err(Error("assertion does not sign this statement"));
    }
    let authenticator: &[u8] = credential.response.authenticator_data.as_ref();
    if authenticator.len() < 37 || authenticator[..32] != Sha256::digest(owner.rp_id.as_bytes())[..]
    {
        return Err(Error("assertion is for another relying party"));
    }
    let flags = authenticator[32];
    if flags & USER_PRESENT == 0 || flags & USER_VERIFIED == 0 {
        return Err(Error("assertion lacks user presence or verification"));
    }
    let mut signed = authenticator.to_vec();
    signed.extend_from_slice(&Sha256::digest(client_data));
    match key
        .public_key
        .verify_signature(credential.response.signature.as_ref(), &signed)
    {
        Ok(true) => Ok(()),
        _ => Err(Error("invalid owner signature")),
    }
}

impl SignedStatement {
    /// Verify against `keys` and return the parsed statement.
    pub fn verify(&self, owner: &Owner, keys: &[OwnerKey]) -> Result<Statement> {
        let statement = Statement::parse(&self.statement)?;
        verify_assertion(owner, keys, &challenge(&self.statement), &self.assertion)?;
        Ok(statement)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterDevice {
    pub public_key: String,
    pub name: String,
    pub approved_at: i64,
}

/// The owner's devices and keys as derived from verified statements only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Roster {
    pub devices: BTreeMap<String, RosterDevice>,
    pub revoked: BTreeSet<String>,
    pub keys: Vec<OwnerKey>,
}

/// Derive the roster. Statements that fail verification are ignored, so a
/// backend can withhold statements but cannot forge or alter them.
/// Revocation is permanent and wins regardless of order; passkey changes
/// apply in order because each must be signed by a key trusted before it.
pub fn roster(owner: &Owner, statements: &[SignedStatement]) -> Roster {
    let mut keys = owner.keys.clone();
    let mut approved = BTreeMap::new();
    let mut revoked = BTreeSet::new();
    for signed in statements {
        let Ok(statement) = signed.verify(owner, &keys) else {
            continue;
        };
        match statement {
            Statement::DeviceApprove {
                device_id,
                public_key,
                name,
                issued_at,
            } => {
                approved.entry(device_id).or_insert(RosterDevice {
                    public_key,
                    name,
                    approved_at: issued_at,
                });
            }
            Statement::DeviceRevoke { device_id, .. } => {
                revoked.insert(device_id);
            }
            Statement::PasskeyAdd { key, .. } => {
                if !keys.iter().any(|k| k.credential_id == key.credential_id) {
                    keys.push(key);
                }
            }
            Statement::PasskeyRemove { credential_id, .. } => {
                // Never remove the last key: that would lock every device out.
                if keys.len() > 1 {
                    keys.retain(|k| k.credential_id != credential_id);
                }
            }
            // Login proofs are one-time and never change the roster.
            Statement::ManagementLogin { .. } => {}
        }
    }
    approved.retain(|id, _| !revoked.contains(id));
    Roster {
        devices: approved,
        revoked,
        keys,
    }
}

/// A software passkey that produces real owner assertions, for tests only.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use openssl::{
        bn::BigNumContext,
        ec::{EcGroup, EcKey, PointConversionForm},
        hash::MessageDigest,
        nid::Nid,
        pkey::{PKey, Private},
        sign::Signer,
    };
    use webauthn_rs::prelude::{COSEAlgorithm, COSEEC2Key, COSEKeyType, ECDSACurve};

    pub struct Authenticator {
        id: Vec<u8>,
        key: PKey<Private>,
    }

    impl Authenticator {
        pub fn new(id: &[u8]) -> Self {
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            Self {
                id: id.to_vec(),
                key: PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap(),
            }
        }

        pub fn owner_key(&self) -> OwnerKey {
            let ec = self.key.ec_key().unwrap();
            let mut context = BigNumContext::new().unwrap();
            let point = ec
                .public_key()
                .to_bytes(ec.group(), PointConversionForm::UNCOMPRESSED, &mut context)
                .unwrap();
            OwnerKey {
                credential_id: self.id.clone().into(),
                public_key: COSEKey {
                    type_: COSEAlgorithm::ES256,
                    key: COSEKeyType::EC_EC2(COSEEC2Key {
                        curve: ECDSACurve::SECP256R1,
                        x: point[1..33].to_vec().into(),
                        y: point[33..65].to_vec().into(),
                    }),
                },
            }
        }

        pub fn assert(&self, owner: &Owner, challenge: &[u8], flags: u8) -> PublicKeyCredential {
            let client = serde_json::json!({
                "type":"webauthn.get","challenge":URL_SAFE_NO_PAD.encode(challenge),
                "origin":owner.origin,"crossOrigin":false
            })
            .to_string();
            let mut authenticator = Sha256::digest(owner.rp_id.as_bytes()).to_vec();
            authenticator.push(flags);
            authenticator.extend_from_slice(&[0, 0, 0, 1]);
            let mut signed = authenticator.clone();
            signed.extend_from_slice(&Sha256::digest(client.as_bytes()));
            let mut signer = Signer::new(MessageDigest::sha256(), &self.key).unwrap();
            signer.update(&signed).unwrap();
            serde_json::from_value(serde_json::json!({
                "id":URL_SAFE_NO_PAD.encode(&self.id),
                "rawId":URL_SAFE_NO_PAD.encode(&self.id),
                "type":"public-key",
                "response":{
                    "authenticatorData":URL_SAFE_NO_PAD.encode(&authenticator),
                    "clientDataJSON":URL_SAFE_NO_PAD.encode(client.as_bytes()),
                    "signature":URL_SAFE_NO_PAD.encode(signer.sign_to_vec().unwrap()),
                    "userHandle":null
                }
            }))
            .unwrap()
        }

        pub fn sign(&self, owner: &Owner, statement: &Statement) -> SignedStatement {
            let text = statement.to_text();
            SignedStatement {
                assertion: self.assert(owner, &challenge(&text), 0x05),
                statement: text,
            }
        }
    }

    pub fn owner(authenticator: &Authenticator) -> Owner {
        owner_for(authenticator, "https://backend.example")
    }

    /// The owner of a backend at `origin` whose only key is `authenticator`.
    pub fn owner_for(authenticator: &Authenticator, origin: &str) -> Owner {
        Owner {
            rp_id: origin
                .split_once("://")
                .map_or(origin, |(_, host)| host)
                .split(':')
                .next()
                .unwrap()
                .into(),
            origin: origin.into(),
            keys: vec![authenticator.owner_key()],
        }
    }

    pub fn approve(name: &str, seed: u8) -> Statement {
        let public_key = STANDARD.encode([seed; 32]);
        Statement::DeviceApprove {
            device_id: device_id(&public_key).unwrap(),
            public_key,
            name: name.into(),
            issued_at: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};

    #[test]
    fn devices_accept_only_statements_the_owner_passkey_signed() {
        let passkey = Authenticator::new(b"owner-key");
        let owner = owner(&passkey);
        let signed = passkey.sign(&owner, &approve("laptop", 7));
        assert_eq!(
            signed.verify(&owner, &owner.keys).unwrap(),
            approve("laptop", 7)
        );
        // Altered text, another statement's assertion, another key, another
        // origin or relying party, and missing user verification all fail.
        let mut altered = signed.clone();
        altered.statement = approve("intruder", 7).to_text();
        assert!(altered.verify(&owner, &owner.keys).is_err());
        let mut swapped = passkey.sign(&owner, &approve("other", 8));
        swapped.statement = signed.statement.clone();
        assert!(swapped.verify(&owner, &owner.keys).is_err());
        let stranger = Authenticator::new(b"owner-key");
        assert!(
            stranger
                .sign(&owner, &approve("laptop", 7))
                .verify(&owner, &owner.keys)
                .is_err()
        );
        for (rp_id, origin) in [
            ("evil.example", "https://backend.example"),
            ("backend.example", "https://evil.example"),
        ] {
            let other = Owner {
                rp_id: rp_id.into(),
                origin: origin.into(),
                keys: owner.keys.clone(),
            };
            assert!(
                passkey
                    .sign(&other, &approve("laptop", 7))
                    .verify(&owner, &owner.keys)
                    .is_err()
            );
        }
        let text = approve("laptop", 7).to_text();
        let unverified = SignedStatement {
            assertion: passkey.assert(&owner, &challenge(&text), USER_PRESENT),
            statement: text,
        };
        assert!(unverified.verify(&owner, &owner.keys).is_err());
    }

    #[test]
    fn statements_reject_mismatched_ids_and_unknown_fields() {
        let mut statement: serde_json::Value =
            serde_json::from_str(&approve("laptop", 7).to_text()).unwrap();
        statement["device_id"] = serde_json::json!("0".repeat(64));
        assert!(Statement::parse(&statement.to_string()).is_err());
        let mut statement: serde_json::Value =
            serde_json::from_str(&approve("laptop", 7).to_text()).unwrap();
        statement["sudo"] = serde_json::json!(true);
        assert!(Statement::parse(&statement.to_string()).is_err());
        assert!(Statement::parse(&approve("", 7).to_text()).is_err());
        assert!(Statement::parse(&approve("a\nb", 7).to_text()).is_err());
    }

    #[test]
    fn roster_ignores_forgeries_and_revocation_is_permanent() {
        let passkey = Authenticator::new(b"owner-key");
        let owner = owner(&passkey);
        let laptop = approve("laptop", 1);
        let Statement::DeviceApprove { device_id, .. } = &laptop else {
            unreachable!()
        };
        let revoke = Statement::DeviceRevoke {
            device_id: device_id.clone(),
            issued_at: 2,
        };
        let forged = Authenticator::new(b"owner-key").sign(&owner, &approve("forged", 3));
        // Revocation listed before the approval still wins.
        let statements = vec![
            passkey.sign(&owner, &revoke),
            passkey.sign(&owner, &laptop),
            passkey.sign(&owner, &approve("server", 2)),
            forged,
        ];
        let roster = roster(&owner, &statements);
        assert_eq!(roster.devices.len(), 1);
        assert_eq!(roster.devices.values().next().unwrap().name, "server");
        assert!(roster.revoked.contains(device_id));
    }

    #[test]
    fn added_passkeys_must_be_endorsed_by_a_trusted_one() {
        let first = Authenticator::new(b"first");
        let second = Authenticator::new(b"second");
        let owner = owner(&first);
        let add = Statement::PasskeyAdd {
            key: second.owner_key(),
            issued_at: 1,
        };
        // A self-endorsed key is ignored, and so is anything it signs.
        let rejected = roster(
            &owner,
            &[
                second.sign(&owner, &add),
                second.sign(&owner, &approve("x", 9)),
            ],
        );
        assert!(rejected.devices.is_empty());
        assert_eq!(rejected.keys.len(), 1);
        let accepted = roster(
            &owner,
            &[
                first.sign(&owner, &add),
                second.sign(&owner, &approve("x", 9)),
            ],
        );
        assert_eq!(accepted.devices.len(), 1);
        assert_eq!(accepted.keys.len(), 2);
        // The last remaining key is never removed.
        let remove = |key: &Authenticator| Statement::PasskeyRemove {
            credential_id: key.owner_key().credential_id,
            issued_at: 2,
        };
        let removed = roster(
            &owner,
            &[
                first.sign(&owner, &add),
                second.sign(&owner, &remove(&first)),
            ],
        );
        assert_eq!(removed.keys, vec![second.owner_key()]);
        let kept = roster(&owner, &[first.sign(&owner, &remove(&first))]);
        assert_eq!(kept.keys.len(), 1);
    }
}
