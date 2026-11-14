//! Offline prerequisite-installation fixtures. Never install software or ask for UAC.
#![cfg(windows)]

use localshelld::process::background_command;
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    time::{Duration, Instant},
};

#[test]
fn official_script_adapter_handles_success_cancellation_failure_and_reboot() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let powershell = localshelld_native_process::system_directory()
        .unwrap()
        .join("WindowsPowerShell/v1.0/powershell.exe");
    for mode in [
        "success",
        "reboot",
        "cancel",
        "msi_failure",
        "download",
        "missing",
        "unsigned",
    ] {
        let directory = tempfile::Builder::new()
            .prefix("trustd MSI fixture ")
            .tempdir()
            .unwrap();
        let output = background_command(&powershell)
            .env_remove("PSModulePath")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(root.join("tests/fixtures/powershell_install.ps1"))
            .arg("-Wrapper")
            .arg(root.join("src/maintenance/powershell.ps1"))
            .arg("-Directory")
            .arg(directory.path())
            .arg("-Mode")
            .arg(mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn prerequisite_download_inherits_proxy_and_does_not_log_proxy_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("download.ps1");
    let mut command = background_command(env!("CARGO_BIN_EXE_trustd"));
    command
        .args([
            "maintenance-powershell-download",
            "https://aka.ms/install-powershell.ps1",
        ])
        .arg(&target)
        .env(
            "HTTPS_PROXY",
            format!("http://fixture-user:fixture-proxy-password@{address}"),
        )
        .env("HTTP_PROXY", "")
        .env("ALL_PROXY", "")
        .env("NO_PROXY", "")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut connection = loop {
        match listener.accept() {
            Ok((connection, _)) => break connection,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("download did not use inherited proxy: {error}");
            }
        }
    };
    connection
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut request = [0u8; 4096];
    let count = connection.read(&mut request).unwrap();
    assert!(String::from_utf8_lossy(&request[..count]).starts_with("CONNECT aka.ms:443"));
    connection
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    drop(connection);
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(!target.exists());
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("fixture-proxy-password"));
    }
}
