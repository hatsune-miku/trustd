//! Copyable, same-port SSH forwarding instructions, not a tunnel launched on
//! the remote host. Keep all suggested or entered arguments safe for sh and
//! PowerShell; SSH aliases can still resolve through the client's own config.
use super::Prompts;
use anyhow::Result;
use std::net::{IpAddr, SocketAddr};

fn destination(value: &str) -> Option<String> {
    let (user, host) = match value.split_once('@') {
        Some((user, host)) => {
            if !safe_name(user) {
                return None;
            }
            (Some(user), host)
        }
        None => (None, value),
    };
    let host = if host.starts_with('[') {
        let address = host.strip_prefix('[')?.strip_suffix(']')?;
        address.parse::<std::net::Ipv6Addr>().ok()?.to_string()
    } else if let Ok(address) = host.parse::<IpAddr>() {
        address.to_string()
    } else if safe_name(host) {
        host.to_owned()
    } else {
        return None;
    };
    Some(match user {
        Some(user) => format!("{user}@{host}"),
        None => host,
    })
}

fn safe_name(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
        && value
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"_.-".contains(&ch))
}

fn port(value: &str) -> Option<u16> {
    value.parse().ok().filter(|port| *port != 0)
}

fn suggestion(connection: Option<&str>, user: Option<&str>) -> Option<(String, u16)> {
    // SSH_CONNECTION: client address, client port, server address, server port.
    // This cannot reveal a public NAT address, ProxyJump route or client alias.
    let parts: Vec<_> = connection?.split_whitespace().collect();
    let [client, client_port, server, server_port] = parts.as_slice() else {
        return None;
    };
    client.parse::<IpAddr>().ok()?;
    port(client_port)?;
    let server = server.parse::<IpAddr>().ok()?;
    let host = match user.filter(|user| safe_name(user)) {
        Some(user) => format!("{user}@{server}"),
        None => server.to_string(),
    };
    Some((host, port(server_port)?))
}

fn command(target: &str, port: Option<u16>, user: SocketAddr, system: SocketAddr) -> String {
    let mut command = String::from("ssh -N -o ExitOnForwardFailure=yes");
    for endpoint in [system, user] {
        // SocketAddr formats IPv6 with brackets for OpenSSH's -L syntax.
        command.push_str(&format!(" -L '127.0.0.1:{}:{endpoint}'", endpoint.port()));
    }
    if let Some(port) = port {
        command.push_str(&format!(" -p {port}"));
    }
    command.push_str(&format!(" '{target}'"));
    command
}

pub(super) fn guide(
    prompts: &mut impl Prompts,
    user: SocketAddr,
    system: SocketAddr,
    connection: Option<&str>,
    login: Option<&str>,
) -> Result<()> {
    let suggested = suggestion(connection, login);
    let choice = if suggested.is_some() { "Y/n" } else { "y/N" };
    loop {
        match prompts
            .line(&format!(
                "生成 SSH 转发命令，在另一台电脑打开管理页？ [{choice}]："
            ))?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "y" | "yes" => break,
            "n" | "no" => return Ok(()),
            "" if suggested.is_some() => break,
            "" => return Ok(()),
            _ => prompts.say("请输入 y 或 n。")?,
        }
    }
    prompts.say("填写平时连接此服务器使用的地址或 SSH 别名；自动识别的地址可能是内网地址。")?;
    let default = suggested
        .as_ref()
        .map_or(String::new(), |(host, _)| format!(" [{host}]"));
    let target = loop {
        let input = prompts.line(&format!("SSH 目标，用户名@主机或别名{default}："))?;
        let input = input.trim();
        let input = if input.is_empty() {
            suggested.as_ref().map_or("", |(host, _)| host)
        } else {
            input
        };
        if let Some(target) = destination(input) {
            break target;
        }
        prompts.say("请输入主机名、IP 地址或 SSH 别名，可在前面加用户名@；端口在下一步填写。")?;
    };
    // A changed target may be an alias with its own Port and User. Do not
    // silently override the client's SSH config with the server-side defaults.
    let default_port = suggested
        .filter(|(host, _)| *host == target)
        .map(|(_, port)| port);
    let prompt = match default_port {
        Some(port) => format!("SSH 端口 [{port}]："),
        None => "SSH 端口，回车沿用本机 SSH 配置：".to_owned(),
    };
    let selected_port = loop {
        let input = prompts.line(&prompt)?;
        if input.trim().is_empty() {
            break default_port;
        }
        if let Some(port) = port(input.trim()) {
            break Some(port);
        }
        prompts.say("请输入 1–65535 之间的端口。")?;
    };
    prompts.say("在有浏览器的电脑上新开终端运行：")?;
    prompts.say(&command(&target, selected_port, user, system))?;
    prompts.say(&format!(
        "用户管理：http://localhost:{}/\n系统管理：http://localhost:{}/",
        user.port(),
        system.port()
    ))?;
    prompts.say(&format!(
        "首次配对：在用户管理页生成配对码，在系统管理页填写 {user} 与配对码。已有配对无需重建。"
    ))?;
    prompts.say("保持 SSH 终端开启，按 Ctrl+C 关闭转发。管理页仍使用服务器的主密码登录。")?;
    prompts.say("若端口被占用，请换一台电脑，或手动停止占用端口的本机服务后重试；目前不支持更换本地转发端口。")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintenance::terminal::tests::Scripted;

    const CONNECTION: &str = "192.0.2.10 54321 192.0.2.20 2222";

    fn run(lines: &[&'static str], connection: Option<&str>) -> Result<String> {
        let mut prompts = Scripted {
            lines: lines.iter().copied().collect(),
            passwords: [].into(),
            output: String::new(),
        };
        guide(
            &mut prompts,
            "127.0.0.1:32120".parse()?,
            "127.0.0.1:32119".parse()?,
            connection,
            Some("miku"),
        )?;
        assert!(prompts.lines.is_empty());
        Ok(prompts.output)
    }

    #[test]
    fn ssh_session_prefills_server_not_client_and_uses_actual_ports() {
        let output = run(&["", "", ""], Some(CONNECTION)).unwrap();
        assert!(output.contains("ssh -N -o ExitOnForwardFailure=yes -L '127.0.0.1:32119:127.0.0.1:32119' -L '127.0.0.1:32120:127.0.0.1:32120' -p 2222 'miku@192.0.2.20'"));
        assert!(!output.contains("192.0.2.10"));
        assert!(output.contains("系统管理页填写 127.0.0.1:32120"));
        assert!(output.contains("目前不支持更换本地转发端口"));
    }

    #[test]
    fn changed_alias_preserves_client_ssh_config() {
        let output = run(&["y", "my-server", ""], Some(CONNECTION)).unwrap();
        assert!(output.contains(" 'my-server'"));
        assert!(!output.contains(" -p "));
        assert!(!output.contains("miku@"));
    }

    #[test]
    fn local_console_can_skip_or_supply_target_and_port() {
        assert!(run(&[""], None).unwrap().is_empty());
        assert!(run(&["n"], Some(CONNECTION)).unwrap().is_empty());
        let output = run(
            &["maybe", "y", "", "-oops", "miku@host", "0", "65536", "2200"],
            None,
        )
        .unwrap();
        assert!(output.contains(" -p 2200 'miku@host'"));
        assert!(run(&[], None).is_err());
        assert!(run(&["y"], None).is_err());
    }

    #[test]
    fn untrusted_suggestions_and_shell_syntax_are_not_copied() {
        for input in [
            "",
            "-R",
            "host name",
            "host;id",
            "$(id)",
            "x'y",
            "x`y",
            "x\ny",
            "x\u{1b}[m",
            "x@@y",
            "-o@host",
            "user@host:22",
            "ssh://host",
        ] {
            assert!(destination(input).is_none(), "{input:?}");
        }
        for connection in [
            "bad",
            "192.0.2.1 22 host 22",
            "192.0.2.1 0 192.0.2.2 22",
            "192.0.2.1 22 192.0.2.2 0",
            "192.0.2.1 22 192.0.2.2 22 extra",
        ] {
            assert!(suggestion(Some(connection), Some("miku")).is_none());
        }
        assert_eq!(
            suggestion(Some(CONNECTION), Some("$(id)")),
            Some(("192.0.2.20".into(), 2222))
        );
    }

    #[test]
    fn ipv6_and_custom_daemon_ports_use_bracketed_forward_targets() {
        let target = destination("miku@[2001:db8::20]").unwrap();
        assert_eq!(target, "miku@2001:db8::20");
        assert_eq!(
            suggestion(Some("2001:db8::10 1234 2001:db8::20 22"), Some("miku")),
            Some((target.clone(), 22))
        );
        assert_eq!(
            command(
                &target,
                None,
                "[::1]:41020".parse().unwrap(),
                "127.0.0.2:41019".parse().unwrap()
            ),
            "ssh -N -o ExitOnForwardFailure=yes -L '127.0.0.1:41019:127.0.0.2:41019' -L '127.0.0.1:41020:[::1]:41020' 'miku@2001:db8::20'"
        );
    }
}
