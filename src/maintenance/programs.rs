//! Fixed-path installation: relocate the maintenance host, stop only managed
//! executables, and retire strictly recognized legacy bundle directories.
use anyhow::{Context, Result, ensure};
use localshelld::paths::{MachinePaths, legacy_digest};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

/// Run before creating the async runtime. The installed maintenance executable
/// cannot remain mapped while Windows replaces it; Unix exec also ensures the
/// installer won't terminate its own UI when retiring the old bundle.
pub(crate) fn relocate() -> Result<bool> {
    let executable = std::env::current_exe()?;
    if !MachinePaths::resolve()?.owns_program(&executable) {
        return Ok(false);
    }
    let source = executable.parent().context("missing bundle directory")?;
    let manifest = crate::bundle::Manifest::read(source)?;
    let stage = tempfile::Builder::new().prefix("trustd-setup-").tempdir()?;
    super::platform::private(stage.path(), true)?;
    manifest.copy(source, stage.path())?;
    let mut command = host_command(
        stage.path(),
        &std::env::args_os().skip(1).collect::<Vec<_>>(),
    )?;
    #[cfg(windows)]
    {
        command
            .spawn()
            .context("could not start temporary maintenance host")?;
        // A system-shell supervisor waits outside the bundle and removes its
        // exact staged files after the host exits (including Windows' mapped exe).
        let _ = stage.keep();
        Ok(true)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // On success exec skips destructors; on failure TempDir cleans up.
        Err(command.exec()).context("could not start temporary maintenance host")
    }
}

fn host_command(stage: &Path, args: &[std::ffi::OsString]) -> Result<std::process::Command> {
    let stage = stage.canonicalize()?;
    ensure!(
        directory(&stage)?,
        "temporary maintenance directory is not a regular directory"
    );
    let executable = stage.join(crate::bundle::filename(crate::bundle::MAINTENANCE));
    #[cfg(windows)]
    {
        use base64::Engine;
        let args = args
            .iter()
            .map(|arg| arg.to_str().context("invalid maintenance argument"))
            .collect::<Result<Vec<_>>>()?;
        let payload = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&serde_json::json!({
            "stage": stage, "program": executable, "args": args,
            "files": crate::bundle::names(true).iter().map(|name| stage.join(name)).collect::<Vec<_>>(),
        }))?);
        // No recursive removal or user-supplied script. Decode literal paths;
        // never interpolate path/argument contents into PowerShell source.
        let script = format!(
            r#"
$ErrorActionPreference = 'Stop'
$data = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{payload}')) | ConvertFrom-Json
$result = 1
try {{
    $arguments = @($data.args)
    & ([string]$data.program) @arguments
    $result = $LASTEXITCODE
}} finally {{
    $item = Get-Item -LiteralPath $data.stage -Force -ErrorAction SilentlyContinue
    if ($item -and -not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {{
        foreach ($file in $data.files) {{ [IO.File]::Delete([string]$file) }}
        try {{ [IO.Directory]::Delete([string]$data.stage, $false) }} catch {{}}
    }}
}}
exit $result
"#
        );
        let mut command = crate::windows_command("WindowsPowerShell/v1.0/powershell.exe")?;
        command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        Ok(command)
    }
    #[cfg(unix)]
    {
        // Arguments are separate argv values, including paths with quotes and
        // shell metacharacters. Cleanup is flat and limited to known role names.
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                r#"
stage=$1
shift
"$@"
result=$?
if [ ! -L "$stage" ]; then
    for name in trustd trustd-user trustd-user-headless trustd-root trustd-stdio; do
        /bin/rm -f -- "$stage/$name"
    done
    /bin/rmdir -- "$stage" 2>/dev/null || :
fi
exit "$result"
"#,
                "trustd-setup",
            ])
            .arg(stage)
            .arg(executable)
            .args(args);
        Ok(command)
    }
}

fn process_kind() -> ProcessRefreshKind {
    // sysinfo otherwise lists every Linux thread as a separate process.
    ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::Always)
        .without_tasks()
}

fn snapshot() -> System {
    System::new_with_specifics(RefreshKind::nothing().with_processes(process_kind()))
}

pub(super) fn stop(paths: &MachinePaths) -> Result<()> {
    ensure!(
        !paths.owns_program(&std::env::current_exe()?),
        "installer must run from temporary staging"
    );
    let started = Instant::now();
    loop {
        let processes = snapshot();
        let installed: Vec<_> = processes
            .processes()
            .values()
            .filter(|p| p.exe().is_some_and(|exe| paths.owns_program(exe)))
            .collect();
        if installed.is_empty() {
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(30),
            "installed trustd instances did not exit; close AI clients and retry installation"
        );
        for process in installed {
            // Revalidate PID identity just before signalling; do not act on a
            // process that has exited/reused its PID since the full snapshot.
            let mut current = System::new();
            current.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[process.pid()]),
                true,
                process_kind(),
            );
            let Some(confirmed) = current.process(process.pid()) else {
                continue;
            };
            if confirmed.start_time() != process.start_time() || confirmed.exe() != process.exe() {
                continue;
            }
            #[cfg(unix)]
            if started.elapsed() < Duration::from_secs(3) {
                confirmed.kill_with(sysinfo::Signal::Term);
                continue;
            }
            confirmed.kill();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn directory(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(metadata.is_dir() && !linked(&metadata))
}

fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(super) fn remove_legacy(paths: &MachinePaths) -> Result<()> {
    let versions = paths.program.join("versions");
    if !versions.try_exists()? {
        return Ok(());
    }
    ensure!(
        directory(&paths.program)? && directory(&versions)?,
        "legacy program directory must not be a symlink/junction"
    );
    let root = paths.program.canonicalize()?;
    let names = crate::bundle::names(cfg!(windows));
    for entry in fs::read_dir(&versions)? {
        let entry = entry?;
        let path = entry.path();
        if !legacy_digest(&entry.file_name().to_string_lossy()) || !directory(&path)? {
            continue;
        }
        // Validate absolute containment and every entry before deleting anything.
        // Never recursively remove an unknown directory or follow a junction.
        ensure!(
            path.canonicalize()?.starts_with(root.join("versions")),
            "legacy path escaped program directory"
        );
        let files = fs::read_dir(&path)?.collect::<std::io::Result<Vec<_>>>()?;
        let recognized = files.iter().all(|file| {
            names.iter().any(|name| file.file_name() == name.as_str())
                && fs::symlink_metadata(file.path())
                    .is_ok_and(|meta| meta.is_file() && !linked(&meta))
        });
        if !recognized {
            tracing::warn!(path = %path.display(), "leaving legacy directory containing unrecognized files");
            continue;
        }
        for file in files {
            fs::remove_file(file.path())?;
        }
        fs::remove_dir(&path)?;
    }
    if fs::read_dir(&versions)?.next().is_none() {
        fs::remove_dir(versions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(program: &Path) -> MachinePaths {
        MachinePaths {
            program: program.to_owned(),
            data: program.join("data"),
            config: program.join("data/config.toml"),
        }
    }

    #[test]
    fn cleanup_removes_only_recognized_legacy_bundles() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let versions = temp.path().join("versions");
        let legacy = versions.join("a".repeat(64));
        let custom = versions.join("b".repeat(64));
        let unknown = versions.join("custom");
        for path in [&legacy, &custom, &unknown] {
            fs::create_dir_all(path).unwrap();
        }
        for name in crate::bundle::names(cfg!(windows)) {
            fs::write(legacy.join(name), b"old").unwrap();
        }
        fs::write(paths.executable(), b"current").unwrap();
        fs::write(custom.join("notes.txt"), b"keep").unwrap();
        fs::write(unknown.join(crate::bundle::filename("trustd")), b"keep").unwrap();
        remove_legacy(&paths).unwrap();
        assert!(!legacy.exists());
        assert_eq!(fs::read(paths.executable()).unwrap(), b"current");
        assert_eq!(fs::read(custom.join("notes.txt")).unwrap(), b"keep");
        assert!(unknown.exists());
    }

    #[test]
    fn staged_host_exits_then_cleans_up_its_own_bundle() {
        let temp = tempfile::Builder::new()
            .prefix("trustd-setup-' $ test-")
            .tempdir()
            .unwrap();
        for name in crate::bundle::names(cfg!(windows)) {
            fs::write(temp.path().join(name), b"fixture").unwrap();
        }
        let executable = temp
            .path()
            .join(crate::bundle::filename(crate::bundle::MAINTENANCE));
        fs::copy(std::env::current_exe().unwrap(), executable).unwrap();
        let status = host_command(temp.path(), &["--list".into()])
            .unwrap()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!temp.path().exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stop_scans_processes_without_their_threads() {
        let (sent, received) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let link = fs::read_link("/proc/thread-self").unwrap();
            let tid = link.file_name().unwrap().to_str().unwrap().parse::<u32>();
            sent.send(tid.unwrap()).unwrap();
            let _ = wait.recv();
        });
        let tid = received.recv().unwrap();
        // One signal per process: a thread listed as its own process would
        // make stop() signal the same installed executable once per thread.
        let system = snapshot();
        assert!(system.process(sysinfo::Pid::from_u32(tid)).is_none());
        assert!(
            system
                .process(sysinfo::Pid::from_u32(std::process::id()))
                .is_some()
        );
        drop(release);
        worker.join().unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn installed_process_fixture() {
        use std::io::Read;
        let _ = std::io::stdin().read(&mut [0]);
    }

    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn spawn(path: &Path) -> Child {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(std::env::current_exe().unwrap(), path).unwrap();
        let mut command = localshelld::process::background_command(path);
        command
            .args([
                "--ignored",
                "--exact",
                "maintenance::programs::tests::installed_process_fixture",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        for _ in 0..20 {
            match command.spawn() {
                Ok(child) => return Child(child),
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => panic!("{error}"),
            }
        }
        panic!("fixture executable remained busy");
    }

    #[test]
    fn stop_matches_paths_not_program_names() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp.path().join("managed"));
        let name = crate::bundle::filename("trustd-stdio");
        let mut current = spawn(&paths.program.join(&name));
        let mut legacy = spawn(
            &paths
                .program
                .join("versions")
                .join("a".repeat(64))
                .join(&name),
        );
        let mut unrelated = spawn(&temp.path().join("unrelated").join(&name));
        stop(&paths).unwrap();
        assert!(current.0.wait().is_ok());
        assert!(legacy.0.wait().is_ok());
        assert!(unrelated.0.try_wait().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_cleanup_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let versions = temp.path().join("versions");
        fs::create_dir(&versions).unwrap();
        let file = outside.path().join(crate::bundle::filename("trustd-stdio"));
        fs::write(&file, b"keep").unwrap();
        symlink(outside.path(), versions.join("c".repeat(64))).unwrap();
        remove_legacy(&paths).unwrap();
        assert_eq!(fs::read(file).unwrap(), b"keep");
    }
}
