use super::string;
use crate::server::AppState;
use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

impl AppState {
    pub(super) async fn remote_file(
        &self,
        owner: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        let session = self
            .remote
            .session(owner, string(args, "session_ref")?)
            .await?;
        self.validate_authorized_session(&session.handle).await?;
        let path = PathBuf::from(string(args, "path")?);
        ensure!(
            path.is_absolute(),
            "file path must be absolute on the target host"
        );
        let args = args.clone();
        let receipts = self.remote.directory()?.join("transfers");
        let operation = operation_owned(operation);
        tokio::task::spawn_blocking(move || match operation {
            FileOperation::Read => super::jobs::read_chunk(path,&args),
            FileOperation::Stat => {
                let metadata = std::fs::metadata(&path)?;
                let checksum = if metadata.is_file() && args["checksum"].as_bool() == Some(true) { Some(checksum(&path)?) } else { None };
                Ok(json!({"path":path,"size":metadata.len(),"directory":metadata.is_dir(),"sha256":checksum}))
            }
            FileOperation::Write => write_chunk(path,&args,&receipts),
        }).await?
    }
}

enum FileOperation {
    Read,
    Stat,
    Write,
}
fn operation_owned(operation: &str) -> FileOperation {
    match operation {
        "file_read" => FileOperation::Read,
        "file_stat" => FileOperation::Stat,
        _ => FileOperation::Write,
    }
}

fn checksum(path: &std::path::Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn write_chunk(path: PathBuf, args: &Value, receipts: &std::path::Path) -> Result<Value> {
    let transfer = uuid::Uuid::parse_str(string(args, "transfer_id")?)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing destination directory"))?;
    ensure!(parent.is_dir(), "destination directory does not exist");
    let stage = parent.join(format!(".trustd-upload-{transfer}"));
    let offset = args["offset"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing offset"))?;
    let bytes = STANDARD.decode(args["data"].as_str().unwrap_or_default())?;
    ensure!(
        bytes.len() <= super::jobs::MAX_BASE64_CHUNK,
        "file chunk exceeds 1 MiB"
    );
    let complete = args["complete"].as_bool() == Some(true);
    std::fs::create_dir_all(receipts)?;
    crate::config::set_private_dir(receipts)?;
    let lock_path = receipts.join(format!("{transfer}.lock"));
    let transfer_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    crate::config::set_private_file(&lock_path)?;
    fs2::FileExt::lock_exclusive(&transfer_lock)?;
    let receipt_path = receipts.join(format!("{transfer}.json"));
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(
            &json!({"path":path,"offset":offset,"data":args["data"],"replace":args["replace"],"sha256":args["sha256"]})
        )?)
    );
    if complete && receipt_path.is_file() {
        let receipt: Value = serde_json::from_slice(&std::fs::read(&receipt_path)?)?;
        ensure!(
            receipt["fingerprint"] == fingerprint,
            "transfer ID already completed with different arguments"
        );
        ensure!(
            checksum(&path)? == receipt["result"]["sha256"],
            "destination changed after upload completed"
        );
        return Ok(receipt["result"].clone());
    }
    if !stage.exists()
        && complete
        && path.is_file()
        && args["sha256"]
            .as_str()
            .is_some_and(|s| checksum(&path).is_ok_and(|actual| actual == s))
    {
        return Ok(
            json!({"complete":true,"size":std::fs::metadata(path)?.len(),"sha256":args["sha256"]}),
        );
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    if offset == 0 {
        options.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&stage)?;
    crate::config::set_private_file(&stage)?;
    fs2::FileExt::lock_exclusive(&file)?;
    let length = file.metadata()?.len();
    ensure!(offset <= length, "upload offset exceeds received data");
    file.seek(SeekFrom::Start(offset))?;
    if offset < length {
        ensure!(
            offset + bytes.len() as u64 <= length,
            "chunk overlaps the end of received data"
        );
        let mut previous = vec![0; bytes.len()];
        file.read_exact(&mut previous)?;
        ensure!(
            previous == bytes,
            "retry payload differs from uploaded bytes"
        );
    } else {
        file.write_all(&bytes)?;
        file.sync_data()?;
    }
    let length = file.metadata()?.len();
    if complete {
        file.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let actual = format!("{:x}", hash.finalize());
        if let Some(expected) = args["sha256"].as_str() {
            ensure!(actual == expected, "uploaded file checksum mismatch");
        }
        drop(file);
        if args["replace"].as_bool() == Some(true) {
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            std::io::copy(&mut std::fs::File::open(&stage)?, &mut temporary)?;
            temporary.as_file().sync_all()?;
            temporary.persist(&path)?;
        } else if let Err(error) = std::fs::hard_link(&stage, &path) {
            // The upload is over either way; do not leave its stage behind.
            let _ = std::fs::remove_file(&stage);
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::bail!("destination exists; pass replace to overwrite it");
            }
            return Err(error.into());
        }
        let result = json!({"complete":true,"size":length,"sha256":actual});
        crate::storage::save_json(
            &receipt_path,
            &json!({"fingerprint":fingerprint,"result":result}),
        )?;
        std::fs::remove_file(&stage)?;
        Ok(result)
    } else {
        Ok(json!({"complete":false,"next_offset":length}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_completion_retries_return_the_same_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("result.bin");
        let receipts = directory.path().join("receipts");
        let args = json!({"transfer_id":uuid::Uuid::new_v4(),"offset":0,"data":STANDARD.encode(b"once"),"complete":true});
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let receipts = receipts.clone();
                let args = args.clone();
                std::thread::spawn(move || write_chunk(path, &args, &receipts).unwrap())
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results[0], results[1]);
        assert_eq!(std::fs::read(path).unwrap(), b"once");
    }

    #[test]
    fn chunk_retries_checksum_failure_and_completion_are_safe() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("result.bin");
        let receipts = directory.path().join("receipts");
        let mut args =
            json!({"transfer_id":uuid::Uuid::new_v4(),"offset":0,"data":STANDARD.encode(b"abc")});
        assert_eq!(
            write_chunk(path.clone(), &args, &receipts).unwrap()["next_offset"],
            3
        );
        assert_eq!(
            write_chunk(path.clone(), &args, &receipts).unwrap()["next_offset"],
            3
        );
        let mut invalid = args.clone();
        invalid["data"] = json!(STANDARD.encode(b"def"));
        assert!(write_chunk(path.clone(), &invalid, &receipts).is_err());
        args["complete"] = json!(true);
        args["sha256"] = json!("wrong");
        assert!(write_chunk(path.clone(), &args, &receipts).is_err());
        assert!(!path.exists());
        args.as_object_mut().unwrap().remove("sha256");
        let result = write_chunk(path.clone(), &args, &receipts).unwrap();
        assert_eq!(write_chunk(path.clone(), &args, &receipts).unwrap(), result);
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        args["transfer_id"] = json!(uuid::Uuid::new_v4());
        args["data"] = json!(STANDARD.encode(b"new"));
        assert!(write_chunk(path.clone(), &args, &receipts).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        args["replace"] = json!(true);
        write_chunk(path.clone(), &args, &receipts).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}
