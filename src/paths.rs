//! Standard machine installation layout; user state deliberately lives elsewhere.
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize)]
pub struct MachinePaths {
    pub program: PathBuf,
    pub data: PathBuf,
    pub config: PathBuf,
}

impl MachinePaths {
    pub fn resolve() -> Result<Self> {
        #[cfg(windows)]
        let (program, data) = (
            localshelld_native_process::program_files_directory()?.join("trustd"),
            localshelld_native_process::program_data_directory()?.join("localshelld"),
        );
        #[cfg(target_os = "macos")]
        let (program, data) = (
            PathBuf::from("/Library/trustd"),
            PathBuf::from("/Library/Application Support/localshelld"),
        );
        #[cfg(target_os = "linux")]
        let (program, data) = (
            PathBuf::from("/opt/trustd"),
            PathBuf::from("/etc/localshelld"),
        );
        let config = data.join("system/config.toml");
        Ok(Self {
            program,
            data,
            config,
        })
    }

    pub fn executable(&self) -> PathBuf {
        self.program.join(if cfg!(windows) {
            "trustd.exe"
        } else {
            "trustd"
        })
    }

    /// Match only a direct installed role, or an old digest directory's role.
    /// No basename-only matching: unrelated programs (including Apple's trustd)
    /// must never be stopped or migrated by the installer.
    pub fn owns_program(&self, path: &Path) -> bool {
        let root = normalized(&self.program) + "/";
        let path = normalized(path);
        let Some(relative) = path.strip_prefix(&root) else {
            return false;
        };
        let parts: Vec<_> = relative.split('/').collect();
        let name = match parts.as_slice() {
            [name] => *name,
            ["versions", digest, name] if legacy_digest(digest) => *name,
            _ => return false,
        };
        let name = if cfg!(windows) {
            let Some(name) = name.strip_suffix(".exe") else {
                return false;
            };
            name
        } else {
            name
        };
        matches!(
            name,
            "trustd" | "trustd-user" | "trustd-user-headless" | "trustd-stdio"
        ) || name
            == if cfg!(windows) {
                "trustd-system"
            } else {
                "trustd-root"
            }
    }

    pub fn contains_system_state(&self, path: &Path) -> bool {
        normalized(path).starts_with(&(normalized(&self.data) + "/"))
            || normalized(path) == normalized(&self.data)
    }
}

pub fn legacy_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text
        .strip_prefix("//?/")
        .unwrap_or(&text)
        .trim_end_matches('/');
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_fixed_and_legacy_role_paths_are_owned() {
        let paths = MachinePaths::resolve().unwrap();
        let name = paths.executable().file_name().unwrap().to_owned();
        assert!(paths.owns_program(&paths.executable()));
        assert!(
            paths.owns_program(
                &paths
                    .program
                    .join("versions")
                    .join("a".repeat(64))
                    .join(&name)
            )
        );
        for value in ["", "../outside", "a/b", "not-a-hash"] {
            assert!(!paths.owns_program(&paths.program.join("versions").join(value).join(&name)));
        }
        assert!(!paths.owns_program(&paths.program.join("other").join(&name)));
        assert!(!paths.owns_program(&paths.program.join("../").join(&name)));
        assert!(!paths.owns_program(&paths.program.join("unknown.exe")));
        assert!(paths.contains_system_state(&paths.config));
        assert!(!paths.contains_system_state(&PathBuf::from(format!(
            "{}-other/config.toml",
            paths.data.display()
        ))));
    }
}
