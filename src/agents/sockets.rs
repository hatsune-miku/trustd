//! macOS ships lsof with a machine-readable socket-owner interface. Keep its
//! parsing separate from process names and never invoke a shell or consult PATH.
use std::{collections::BTreeSet, net::SocketAddr};

#[cfg(target_os = "macos")]
pub(super) fn socket_owners(peer: SocketAddr, local: SocketAddr) -> Option<BTreeSet<u32>> {
    use std::{
        io::Read,
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut child = crate::process::background_command("/usr/sbin/lsof")
        .args([
            "-nP",
            "-b",
            "-a",
            &format!("-iTCP:{}", peer.port()),
            "-sTCP:ESTABLISHED",
            "-F0pn",
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = vec![];
    child
        .stdout
        .take()?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut output)
        .ok()?;
    if output.len() > 1024 * 1024 {
        return None;
    }
    parse_owners(&output, peer, local)
}

fn parse_owners(output: &[u8], peer: SocketAddr, local: SocketAddr) -> Option<BTreeSet<u32>> {
    let output = std::str::from_utf8(output).ok()?;
    let expected = format!("{peer}->{local}");
    let mut pid = None;
    let mut owners = BTreeSet::new();
    for field in output.split('\0') {
        let field = field.trim_start_matches('\n');
        if let Some(value) = field.strip_prefix('p') {
            pid = value.parse::<u32>().ok().filter(|pid| *pid != 0);
        } else if field.strip_prefix('n') == Some(expected.as_str()) {
            owners.insert(pid?);
        }
    }
    (!owners.is_empty()).then_some(owners)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_owner_requires_the_exact_direction_and_both_addresses() {
        let peer = "127.0.0.1:4444".parse().unwrap();
        let local = "127.0.0.1:32120".parse().unwrap();
        let output = b"p12\0\nn127.0.0.1:32120->127.0.0.1:4444\0\np34\0\nn127.0.0.1:4444->127.0.0.1:32120\0\np56\0\nn127.0.0.1:4444->127.0.0.1:32119\0\n";
        assert_eq!(
            parse_owners(output, peer, local),
            Some(BTreeSet::from([34]))
        );
        assert!(parse_owners(b"n127.0.0.1:4444->127.0.0.1:32120\0", peer, local).is_none());
        let shared =
            b"p34\0\nn127.0.0.1:4444->127.0.0.1:32120\0\np35\0\nn127.0.0.1:4444->127.0.0.1:32120\0";
        assert_eq!(parse_owners(shared, peer, local).unwrap().len(), 2);
        assert_eq!(
            parse_owners(
                b"p34\0\nn[::1]:4444->[::1]:32120\0\n",
                "[::1]:4444".parse().unwrap(),
                "[::1]:32120".parse().unwrap()
            ),
            Some(BTreeSet::from([34]))
        );
    }
}
