use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) fn redact(
    text: &str,
    handle: &str,
    environment: Option<&crate::secrets::Environment>,
) -> String {
    let mut values: Vec<_> = environment
        .into_iter()
        .flat_map(|environment| environment.0.iter())
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (value.as_str(), format!("[credential:{key}]")))
        .collect();
    if !handle.is_empty() {
        values.push((handle, "[session]".into()));
    }
    values.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    values
        .into_iter()
        .fold(text.to_owned(), |text, (value, label)| {
            text.replace(value, &label)
        })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Command,
    Audit,
}

impl Kind {
    fn filename(self) -> &'static str {
        match self {
            Self::Command => "command.log",
            Self::Audit => "audit.log",
        }
    }
}

#[derive(Default)]
pub(crate) struct Logs {
    directory: Option<PathBuf>,
    gate: Mutex<()>,
}

impl Logs {
    pub fn new(config: &std::path::Path) -> Self {
        Self {
            directory: Some(
                config
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_owned(),
            ),
            gate: Mutex::new(()),
        }
    }

    pub async fn record(self: &Arc<Self>, kind: Kind, event: &str, mut data: Value) {
        if self.directory.is_none() {
            return;
        }
        data["event"] = json!(event);
        data["timestamp"] =
            json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        let logs = self.clone();
        let result = tokio::task::spawn_blocking(move || logs.append(kind, &data)).await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(?kind, "could not append local log");
        }
    }

    fn append(&self, kind: Kind, data: &Value) -> Result<()> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("log lock failed"))?;
        let path = self
            .directory
            .as_ref()
            .expect("persistent log directory")
            .join(kind.filename());
        if !path.try_exists()? {
            crate::storage::write_private(&path, b"")?;
        }
        let metadata = fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.is_symlink(),
            "log must be a regular file"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            anyhow::ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "log must not be redirected"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(metadata.nlink() == 1, "log must not be hard-linked");
        }
        crate::config::set_private_file(&path)?;
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;
        if metadata.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        let mut line = serde_json::to_vec(data)?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    }

    pub async fn page(
        self: &Arc<Self>,
        kind: Kind,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Value> {
        let logs = self.clone();
        tokio::task::spawn_blocking(move || logs.read_page(kind, before, limit)).await?
    }

    fn read_page(&self, kind: Kind, before: Option<u64>, limit: usize) -> Result<Value> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("log lock failed"))?;
        let Some(directory) = &self.directory else {
            return Ok(json!({"entries":[],"next_before":null}));
        };
        let mut file = match fs::File::open(directory.join(kind.filename())) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({"entries":[],"next_before":null}));
            }
            Err(error) => return Err(error.into()),
        };
        let length = file.metadata()?.len();
        let end = before.unwrap_or(length).min(length);
        let mut position = end;
        let mut chunks = Vec::new();
        let mut lines = 0;
        while position > 0 && lines <= limit {
            let length = position.min(64 * 1024) as usize;
            position -= length as u64;
            file.seek(SeekFrom::Start(position))?;
            let mut bytes = vec![0; length];
            file.read_exact(&mut bytes)?;
            lines += bytes.iter().filter(|byte| **byte == b'\n').count();
            chunks.push(bytes);
        }
        chunks.reverse();
        let bytes = chunks.concat();
        let mut cursor = end;
        let mut entries = Vec::new();
        for line in bytes.split_inclusive(|byte| *byte == b'\n').rev() {
            let start = cursor - line.len() as u64;
            if start == position && position > 0 {
                break;
            }
            cursor = start;
            if let Ok(entry) = serde_json::from_slice::<Value>(line) {
                entries.push(entry);
                if entries.len() == limit {
                    break;
                }
            }
        }
        Ok(json!({"entries":entries,"next_before":(cursor > 0).then_some(cursor)}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_records_mask_bearers_and_overlapping_credential_values() {
        let environment = crate::secrets::Environment(std::collections::BTreeMap::from([
            ("A".into(), "fixture-secret".into()),
            ("B".into(), "fixture-secret-longer".into()),
        ]));
        let result = redact(
            "use fixture-secret-longer; session-bearer",
            "session-bearer",
            Some(&environment),
        );
        assert_eq!(result, "use [credential:B]; [session]");
    }

    #[tokio::test]
    async fn pages_preserve_multiline_commands_and_do_not_repeat_entries() {
        let directory = tempfile::tempdir().unwrap();
        let logs = Arc::new(Logs::new(&directory.path().join("config.toml")));
        let command = format!("{}\nfirst\nsecond", "x".repeat(100_000));
        for index in 0..5 {
            logs.record(
                Kind::Command,
                "command.finished",
                json!({"index":index,"command":command}),
            )
            .await;
        }
        let page = logs.page(Kind::Command, None, 2).await.unwrap();
        assert_eq!(page["entries"][0]["index"], 4);
        assert_eq!(page["entries"][1]["command"], command);
        let older = logs
            .page(Kind::Command, page["next_before"].as_u64(), 3)
            .await
            .unwrap();
        assert_eq!(older["entries"][0]["index"], 2);
        assert_eq!(older["entries"].as_array().unwrap().len(), 3);
        assert!(older["next_before"].is_null());
        assert!(
            logs.page(Kind::Audit, None, 10).await.unwrap()["entries"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn append_recovers_after_an_incomplete_last_record() {
        let directory = tempfile::tempdir().unwrap();
        let logs = Arc::new(Logs::new(&directory.path().join("config.toml")));
        logs.record(Kind::Audit, "first", json!({})).await;
        OpenOptions::new()
            .append(true)
            .open(directory.path().join("audit.log"))
            .unwrap()
            .write_all(b"{\"incomplete\":")
            .unwrap();
        logs.record(Kind::Audit, "second", json!({})).await;
        let page = logs.page(Kind::Audit, None, 10).await.unwrap();
        assert_eq!(page["entries"].as_array().unwrap().len(), 2);
        assert_eq!(page["entries"][0]["event"], "second");
        assert_eq!(page["entries"][1]["event"], "first");
    }
}
