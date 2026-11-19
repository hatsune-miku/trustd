//! Copy files between this device and a remote one without the bytes passing
//! through the caller: this daemon reads or writes the local file and moves it
//! over the relay with the target's own `file_read` and `file_write`.
use super::{jobs::MAX_BASE64_CHUNK, string};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use trustd_remote::Node;

/// Devices before 0.2.0-rc.95 accept at most this much per upload chunk.
const LEGACY_CHUNK: usize = 48 * 1024;

fn local_path(args: &Value) -> Result<PathBuf> {
    let path = PathBuf::from(string(args, "local_path")?);
    ensure!(
        path.is_absolute(),
        "local_path must be absolute on this device"
    );
    Ok(path)
}

/// Upload `local_path` to `path` on the target.
pub(super) async fn push(node: &Node, target: &str, args: &Value) -> Result<Value> {
    let session = string(args, "session_ref")?;
    let path = string(args, "path")?;
    let local = local_path(args)?;
    let mut file = tokio::fs::File::open(&local)
        .await
        .with_context(|| format!("cannot open {}", local.display()))?;
    let metadata = file.metadata().await?;
    ensure!(metadata.is_file(), "local_path is not a regular file");
    let size = metadata.len();
    let replace = args["replace"].as_bool() == Some(true);
    if !replace {
        // Fail before sending anything rather than after the last chunk.
        let stat = json!({"operation":"file_stat","arguments":{"session_ref":session,"path":path}});
        ensure!(
            node.call(target, stat).await.is_err(),
            "path exists on the target; pass replace:true to overwrite it"
        );
    }
    let transfer = uuid::Uuid::new_v4().to_string();
    let mut hash = Sha256::new();
    let mut chunk = MAX_BASE64_CHUNK;
    let mut offset = 0u64;
    loop {
        let mut bytes = Vec::with_capacity(chunk);
        (&mut file)
            .take(chunk.min((size - offset) as usize) as u64)
            .read_to_end(&mut bytes)
            .await?;
        let end = offset + bytes.len() as u64;
        ensure!(
            !bytes.is_empty() || end == size,
            "local file shrank during upload"
        );
        let complete = end == size;
        let mut arguments = json!({
            "session_ref": session, "path": path, "transfer_id": transfer,
            "offset": offset, "data": STANDARD.encode(&bytes),
            "complete": complete, "replace": replace,
        });
        let mut next = hash.clone();
        next.update(&bytes);
        if complete {
            // The target verifies the whole file before moving it into place.
            arguments["sha256"] = json!(format!("{:x}", next.clone().finalize()));
        }
        match node
            .call(
                target,
                json!({"operation":"file_write","arguments":arguments}),
            )
            .await
        {
            Ok(result) if complete => {
                return Ok(json!({"path":path,"size":size,"sha256":result["sha256"]}));
            }
            Ok(_) => {
                hash = next;
                offset = end;
            }
            Err(error) if chunk > LEGACY_CHUNK && error.to_string().contains("48 KiB") => {
                chunk = LEGACY_CHUNK;
                file.seek(std::io::SeekFrom::Start(offset)).await?;
            }
            Err(error) => {
                return Err(error.context(format!("upload stopped at byte {offset} of {size}")));
            }
        }
    }
}

/// Download `path` from the target to `local_path`.
pub(super) async fn pull(node: &Node, target: &str, args: &Value) -> Result<Value> {
    let session = string(args, "session_ref")?;
    let path = string(args, "path")?;
    let local = local_path(args)?;
    let replace = args["replace"].as_bool() == Some(true);
    ensure!(
        replace || !local.exists(),
        "local_path exists; pass replace:true to overwrite it"
    );
    let parent = local
        .parent()
        .filter(|parent| parent.is_dir())
        .context("local_path's directory does not exist")?;
    let stat = node
        .call(
            target,
            json!({"operation":"file_stat","arguments":{"session_ref":session,"path":path,"checksum":true}}),
        )
        .await?;
    ensure!(stat["directory"] != true, "remote path is a directory");
    let size = stat["size"].as_u64().context("invalid file_stat reply")?;
    let expected = stat["sha256"]
        .as_str()
        .context("invalid file_stat reply")?
        .to_owned();
    // Writing beside the destination keeps the final rename atomic.
    let staged = tempfile::NamedTempFile::new_in(parent)?;
    let mut file = tokio::fs::File::from_std(staged.reopen()?);
    let mut hash = Sha256::new();
    let mut offset = 0u64;
    while offset < size {
        let chunk = node
            .call(
                target,
                json!({"operation":"file_read","arguments":{
                    "session_ref":session,"path":path,"offset":offset,
                    "limit":MAX_BASE64_CHUNK,"encoding":"base64"}}),
            )
            .await
            .with_context(|| format!("download stopped at byte {offset} of {size}"))?;
        let bytes = STANDARD.decode(chunk["data"].as_str().context("invalid file_read reply")?)?;
        ensure!(!bytes.is_empty(), "remote file shrank during download");
        hash.update(&bytes);
        file.write_all(&bytes).await?;
        offset += bytes.len() as u64;
    }
    ensure!(
        format!("{:x}", hash.finalize()) == expected,
        "remote file changed during download; try again"
    );
    file.sync_all().await?;
    drop(file);
    if replace {
        staged.persist(&local)?;
    } else {
        staged.persist_noclobber(&local)?;
    }
    Ok(json!({"local_path":local,"size":size,"sha256":expected}))
}
