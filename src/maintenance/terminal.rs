//! Linux's interactive frontend shares the same digest-bound installer as the
//! browser. Prompts use the controlling TTY, including when bootstrap is piped.
mod ssh;

use super::agents;
use anyhow::Result;

#[cfg(target_os = "linux")]
use anyhow::{Context, bail, ensure};

#[cfg(target_os = "linux")]
pub(crate) fn requested(explicit: bool, no_open: bool) -> bool {
    use std::env::var_os;
    select_terminal(
        explicit,
        no_open,
        var_os("DISPLAY").is_some_and(|value| !value.is_empty()),
        var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()),
    )
}

fn select_terminal(explicit: bool, no_open: bool, display: bool, wayland: bool) -> bool {
    !no_open && (explicit || (!display && !wayland))
}

trait Prompts {
    fn say(&mut self, message: &str) -> Result<()>;
    fn line(&mut self, prompt: &str) -> Result<String>;
}

struct Options {
    /// The owner's backend to join after installation; none once joined.
    backend: Option<String>,
    device_name: Option<String>,
    agents: Vec<String>,
    replace_agents: bool,
}

fn confirm(prompts: &mut impl Prompts, message: &str) -> Result<bool> {
    loop {
        match prompts
            .line(&format!("{message} [y/N]："))?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "y" | "yes" => return Ok(true),
            "" | "n" | "no" => return Ok(false),
            _ => prompts.say("请输入 y 或 n。")?,
        }
    }
}

fn selection(input: &str, catalog: &[agents::Agent]) -> Result<Vec<String>> {
    if input.trim().is_empty() {
        return Ok(vec![]);
    }
    let mut selected = vec![];
    for entry in input
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|s| !s.is_empty())
    {
        let index = entry.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
        let agent = index
            .and_then(|n| catalog.get(n))
            .ok_or_else(|| anyhow::anyhow!("请输入列表中的编号。"))?;
        selected.push(agent.id.to_owned());
    }
    anyhow::ensure!(!selected.is_empty(), "请输入列表中的编号，或直接回车跳过。");
    agents::validate(&selected, catalog)?;
    Ok(selected)
}

fn collect(
    prompts: &mut impl Prompts,
    joined: Option<&str>,
    default_name: &str,
    catalog: &[agents::Agent],
) -> Result<Option<Options>> {
    // There is no standalone mode: a device that has not joined must name its
    // owner's backend. No password is set; the owner's passkey signs in.
    let (backend, device_name) = if let Some(url) = joined {
        prompts.say(&format!("已加入后端 {url}，保留现有授权配置。"))?;
        (None, None)
    } else {
        prompts.say("安装后，此设备将加入你的后端，由通行密钥批准。")?;
        let url = loop {
            let input = prompts.line("后端地址（https://…）：")?;
            let input = input.trim().trim_end_matches('/');
            if input.starts_with("https://") || input.starts_with("http://localhost") {
                break input.to_owned();
            }
            prompts.say("请输入以 https:// 开头的后端地址。")?;
        };
        let name = prompts.line(&format!("设备名称 [{default_name}]："))?;
        let name = name.trim();
        let name = if name.is_empty() {
            default_name.to_owned()
        } else {
            name.chars().take(80).collect()
        };
        (Some(url), Some(name))
    };
    prompts.say("选择要接入的 AI 客户端：")?;
    for (index, agent) in catalog.iter().enumerate() {
        prompts.say(&format!(
            "  {}. {}{}\n     {}",
            index + 1,
            agent.name,
            if agent.detected {
                " · 已有配置"
            } else {
                ""
            },
            agent.path.display()
        ))?;
    }
    let selected = loop {
        let input = prompts.line("编号用空格或逗号分隔，直接回车跳过：")?;
        match selection(&input, catalog) {
            Ok(value) => break value,
            Err(error) => prompts.say(&error.to_string())?,
        }
    };
    let replace_agents =
        !selected.is_empty() && confirm(prompts, "替换所选客户端已有的 trustd 项？原配置会备份。")?;
    if !confirm(prompts, "安装或修复系统和用户服务，并写入所选客户端配置？")?
    {
        prompts.say("已取消，未执行安装。")?;
        return Ok(None);
    }
    Ok(Some(Options {
        backend,
        device_name,
        agents: selected,
        replace_agents,
    }))
}

#[cfg(target_os = "linux")]
struct Terminal {
    input: std::io::BufReader<std::fs::File>,
    output: std::fs::File,
}

#[cfg(target_os = "linux")]
impl Terminal {
    fn open() -> Result<Self> {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context(
                "终端向导需要交互终端；SSH 请使用 ssh -t。网页入口可使用 maintenance --no-open。",
            )?;
        Ok(Self {
            input: std::io::BufReader::new(tty.try_clone()?),
            output: tty,
        })
    }
}

#[cfg(target_os = "linux")]
impl Prompts for Terminal {
    fn say(&mut self, message: &str) -> Result<()> {
        use std::io::Write;
        writeln!(self.output, "{message}")?;
        self.output.flush()?;
        Ok(())
    }

    fn line(&mut self, prompt: &str) -> Result<String> {
        use std::io::Write;
        write!(self.output, "{prompt}")?;
        self.output.flush()?;
        super::super::read_input_line(&mut self.input).context("终端输入已结束。")
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn run(mode: Option<bool>) -> Result<()> {
    use super::{Operation, execute, platform, prepare};
    super::super::require_user().context("请以普通登录用户运行向导，不要在命令前加 sudo。")?;
    let mut terminal = Terminal::open()?;
    terminal.say(&format!("trustd {} · 终端安装向导", localshelld::VERSION))?;
    platform::Elevation::Terminal.check()?;
    ensure!(
        std::path::Path::new("/run/systemd/system").is_dir(),
        "终端向导需要运行中的 systemd。"
    );
    // Fail before initializing configuration or installing the system service.
    // SSH logins with pam_systemd work; no graphical session is required.
    let user_manager = std::process::Command::new("/usr/bin/systemctl")
        .args(["--user", "show-environment"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    ensure!(
        user_manager.success(),
        "当前用户的 systemd 服务管理器不可用。请通过该用户的正常登录或 SSH 会话运行，并检查 systemd 用户服务。"
    );
    let state = prepare(String::new(), mode)?;
    terminal.say(if state.user_headless {
        "用户守护进程：无头模式"
    } else {
        "用户守护进程：桌面模式"
    })?;
    let registration = super::super::update::service::registration()?;
    let system_initialized = super::apply::configuration_source(
        &state.paths,
        registration.as_ref().map(|value| value.config.as_path()),
    )
    .is_ok_and(|source| source.is_some());
    terminal.say(&format!(
        "程序目录：{}\n系统配置：{}\n用户配置：{}",
        state.paths.program.display(),
        state.paths.config.display(),
        state.user_config.display()
    ))?;
    let _ = system_initialized;
    let (joined, backend) = super::joined(&state.user_config);
    let Some(options) = collect(
        &mut terminal,
        backend.as_deref().filter(|_| joined),
        &super::super::joining::device_name(),
        &state.agents,
    )?
    else {
        return Ok(());
    };
    terminal.say("正在安装；sudo 可能要求输入当前账号密码。")?;
    let outcomes = execute(
        &state,
        Operation::Install,
        &options.agents,
        options.replace_agents,
        platform::Elevation::Terminal,
    )?;
    let mut failed = false;
    for outcome in outcomes {
        if let Some(error) = outcome.error {
            failed = true;
            terminal.say(&format!("{}：{error}", outcome.name))?;
        } else {
            terminal.say(&format!(
                "{}：已配置 {}",
                outcome.name,
                outcome.path.display()
            ))?;
        }
        if let Some(backup) = outcome.backup {
            terminal.say(&format!("备份：{}", backup.display()))?;
        }
    }
    if let Some(url) = options.backend {
        terminal.say("在任意浏览器（例如手机）打开下列链接，按一次通行密钥批准此设备：")?;
        let joined = super::super::joining::join_with(
            url.clone(),
            options.device_name,
            Some(state.user_config.clone()),
            |uri, code| {
                let _ = terminal.say(&format!("{uri}\n验证码：{code}"));
            },
        );
        match joined {
            Ok(()) => terminal.say("已加入后端，此设备与你的其他设备互相信任。")?,
            Err(error) => terminal.say(&format!(
                "服务已安装，但未加入后端：{error:#}\n可稍后运行 trustd join {url} 重试。"
            ))?,
        }
    }
    let user = localshelld::config::Config::load(&state.user_config)?;
    terminal.say(&format!(
        "系统和用户服务已启动并已自动配对。\n管理页：http://localhost:{}/",
        user.bind.port()
    ))?;
    // This optional, read-only step runs after successful service installation.
    // EOF here must not report that the already completed installation failed.
    if let Err(error) = ssh::guide(
        &mut terminal,
        user.bind,
        std::env::var("SSH_CONNECTION").ok().as_deref(),
        std::env::var("USER").ok().as_deref(),
    ) {
        terminal.say(&format!("服务已安装，SSH 转发指引未完成：{error}"))?;
    }
    if failed {
        bail!("服务安装成功，但部分客户端配置失败；修复后可重新运行向导。");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    pub(super) struct Scripted {
        pub(super) lines: VecDeque<&'static str>,
        pub(super) output: String,
    }

    impl Prompts for Scripted {
        fn say(&mut self, text: &str) -> Result<()> {
            self.output.push_str(text);
            Ok(())
        }
        fn line(&mut self, _: &str) -> Result<String> {
            self.lines
                .pop_front()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("EOF"))
        }
    }

    fn catalog() -> Vec<agents::Agent> {
        ["codex", "gemini"]
            .into_iter()
            .map(|id| agents::Agent {
                id,
                name: id,
                path: format!("/fixture/{id}").into(),
                detected: false,
            })
            .collect()
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "driven through a pseudo-terminal by scripts/test_terminal.py"]
    fn native_tty_fixture() {
        // Exercise real /dev/tty and password echo handling, but never execute
        // installation, elevation, configuration writes or service management.
        let mut terminal = Terminal::open().unwrap();
        if let Some(options) = collect(&mut terminal, None, "fixture-host", &catalog()).unwrap() {
            assert_eq!(options.backend.as_deref(), Some("https://backend.example"));
            assert_eq!(options.device_name.as_deref(), Some("tty 设备"));
            assert_eq!(options.agents, ["codex"]);
            assert!(!options.replace_agents);
            terminal.say("TTY_FIXTURE_CONFIRMED").unwrap();
        } else {
            terminal.say("TTY_FIXTURE_CANCELLED").unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "driven through a pseudo-terminal by scripts/test_terminal.py"]
    fn native_ssh_guide_fixture() {
        ssh::guide(
            &mut Terminal::open().unwrap(),
            "127.0.0.1:32120".parse().unwrap(),
            Some("192.0.2.10 54321 192.0.2.20 2222"),
            Some("miku"),
        )
        .unwrap();
    }

    #[test]
    fn mode_respects_explicit_cli_and_no_open() {
        assert!(select_terminal(false, false, false, false));
        assert!(select_terminal(true, false, true, true));
        assert!(!select_terminal(false, false, true, false));
        assert!(!select_terminal(false, false, false, true));
        assert!(!select_terminal(false, true, false, false));
    }

    #[test]
    fn only_explicit_unique_agent_numbers_are_accepted() {
        let agents = catalog();
        assert_eq!(selection("1, 2", &agents).unwrap(), ["codex", "gemini"]);
        assert!(selection("", &agents).unwrap().is_empty());
        for input in [
            "0",
            "3",
            "1 1",
            "all",
            "-1",
            "../path",
            "18446744073709551616",
            ", ,",
        ] {
            assert!(selection(input, &agents).is_err(), "{input}");
        }
    }

    #[test]
    fn new_installation_requires_a_backend_and_defaults_the_device_name() {
        let mut prompt = Scripted {
            lines: [
                "ftp://backend",
                "https://backend.example/",
                "",
                "1,2",
                "n",
                "y",
            ]
            .into(),
            output: String::new(),
        };
        let options = collect(&mut prompt, None, "fixture-host", &catalog())
            .unwrap()
            .unwrap();
        assert_eq!(options.backend.as_deref(), Some("https://backend.example"));
        assert_eq!(options.device_name.as_deref(), Some("fixture-host"));
        assert_eq!(options.agents, ["codex", "gemini"]);
        assert!(!options.replace_agents);
        assert!(prompt.output.contains("https://"));
    }

    #[test]
    fn repair_of_a_joined_device_skips_the_backend_and_requires_replacement_consent() {
        let mut prompt = Scripted {
            lines: ["0", "2", "y", "y"].into(),
            output: String::new(),
        };
        let options = collect(
            &mut prompt,
            Some("https://backend.example"),
            "fixture-host",
            &catalog(),
        )
        .unwrap()
        .unwrap();
        assert!(options.backend.is_none());
        assert_eq!(options.agents, ["gemini"]);
        assert!(options.replace_agents);
    }

    #[test]
    fn default_confirmation_and_eof_do_not_authorize_installation() {
        let mut prompt = Scripted {
            lines: ["", ""].into(),
            output: String::new(),
        };
        let joined = Some("https://backend.example");
        assert!(
            collect(&mut prompt, joined, "fixture-host", &catalog())
                .unwrap()
                .is_none()
        );
        assert!(collect(&mut prompt, joined, "fixture-host", &catalog()).is_err());
    }
}
