//! Platform-specific execution choices; never silently substitute identities.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivilegeLevel {
    #[serde(rename = "current-user")]
    CurrentUser,
    #[serde(rename = "as-administrator")]
    Administrator,
    #[serde(rename = "nt-authority/system")]
    System,
    #[serde(rename = "root")]
    Root,
}
impl PrivilegeLevel {
    pub fn privileged() -> Self {
        if cfg!(windows) {
            Self::System
        } else {
            Self::Root
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::CurrentUser => "current-user",
            Self::Administrator => "as-administrator",
            Self::System => "nt-authority/system",
            Self::Root => "root",
        }
    }
    pub fn from_sudo(sudo: bool) -> Self {
        if sudo {
            Self::privileged()
        } else {
            Self::CurrentUser
        }
    }
    pub fn is_privileged(self) -> bool {
        self != Self::CurrentUser
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    pub stdin: String,
    pub timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnterOptions {
    #[serde(default)]
    pub sudo: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl EnterOptions {
    pub(crate) fn validate(mut self) -> Result<Self, crate::server::ApiError> {
        self.cwd = self.cwd.filter(|value| !value.is_empty());
        if self
            .cwd
            .as_ref()
            .is_some_and(|value| value.contains('\0') || !std::path::Path::new(value).is_absolute())
        {
            return Err(crate::server::ApiError::bad_request(
                "cwd must be an absolute path, empty, or null",
            ));
        }
        Ok(self)
    }
}

pub fn current_identity() -> std::io::Result<Value> {
    #[cfg(windows)]
    {
        Ok(serde_json::to_value(
            localshelld_native_process::current_identity()?,
        )?)
    }
    #[cfg(unix)]
    {
        let uid = crate::process::background_command("/usr/bin/id")
            .arg("-u")
            .output()?;
        let user = crate::process::background_command("/usr/bin/id")
            .arg("-un")
            .output()?;
        if !uid.status.success() || !user.status.success() {
            return Err(std::io::Error::other("cannot determine execution identity"));
        }
        let uid: u32 = String::from_utf8_lossy(&uid.stdout)
            .trim()
            .parse()
            .map_err(std::io::Error::other)?;
        Ok(
            json!({"user":String::from_utf8_lossy(&user.stdout).trim(), "uid":uid, "elevated":uid == 0, "is_system":false}),
        )
    }
}

pub fn matches_identity(level: PrivilegeLevel, identity: &Value) -> bool {
    let elevated = identity["elevated"].as_bool().unwrap_or(false);
    let restricted_user = cfg!(windows)
        && identity["restricted_user"].as_bool() == Some(true)
        && identity["integrity"].as_str() == Some("Medium");
    let system = identity["is_system"].as_bool().unwrap_or(false);
    match level {
        PrivilegeLevel::CurrentUser => (!elevated || restricted_user) && !system,
        PrivilegeLevel::Administrator => cfg!(windows) && elevated && !restricted_user && !system,
        PrivilegeLevel::System => cfg!(windows) && system,
        PrivilegeLevel::Root => cfg!(unix) && identity["uid"] == 0,
    }
}

pub fn execution_argument(level: PrivilegeLevel) -> Value {
    if cfg!(windows) {
        json!({"privilege_level":level.name()})
    } else {
        json!({"sudo":level.is_privileged()})
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn restricted_user_identity_is_not_an_administrator_or_system_identity() {
        let mut identity =
            json!({"elevated":true,"restricted_user":true,"integrity":"Medium","is_system":false});
        assert!(matches_identity(PrivilegeLevel::CurrentUser, &identity));
        assert!(!matches_identity(PrivilegeLevel::Administrator, &identity));
        assert!(!matches_identity(PrivilegeLevel::System, &identity));
        identity["restricted_user"] = json!(false);
        assert!(!matches_identity(PrivilegeLevel::CurrentUser, &identity));
        assert!(matches_identity(PrivilegeLevel::Administrator, &identity));
        identity["restricted_user"] = json!(true);
        identity["integrity"] = json!("High");
        assert!(!matches_identity(PrivilegeLevel::CurrentUser, &identity));
        identity["integrity"] = json!("Medium");
        identity["is_system"] = json!(true);
        assert!(!matches_identity(PrivilegeLevel::CurrentUser, &identity));
    }
}
