use super::*;

fn command() -> PathBuf {
    std::env::current_exe().unwrap()
}
fn endpoint() -> SocketAddr {
    "127.0.0.1:43210".parse().unwrap()
}
fn json(source: &str) -> serde_json::Value {
    jsonc_parser::parse_to_serde_value(source.trim_start_matches('\u{feff}'), &Default::default())
        .unwrap()
}

#[test]
fn jsonc_preserves_comments_unrelated_settings_and_uses_the_right_schema() {
    for id in [
        "claude_code",
        "claude_desktop",
        "cursor",
        "windsurf",
        "vscode",
        "gemini",
        "devin",
    ] {
        let key = if id == "vscode" {
            "servers"
        } else {
            "mcpServers"
        };
        let input = format!(
            "\u{feff}{{\r\n // 保留注释\r\n \"preferences\": {{\"keep\": true}},\r\n \"{key}\": {{\"other\": {{\"command\": \"other\"}},}},\r\n}}\r\n"
        );
        let output = render(&input, id, &command(), endpoint(), false).unwrap();
        assert!(output.starts_with('\u{feff}'));
        assert!(output.contains("// 保留注释\r\n"));
        let value = json(&output);
        assert_eq!(value["preferences"]["keep"], true);
        assert_eq!(value[key]["other"]["command"], "other");
        assert_eq!(value[key]["trustd"]["command"], command().to_str().unwrap());
        assert_eq!(
            value[key]["trustd"]["args"],
            serde_json::json!(["--endpoint", "127.0.0.1:43210"])
        );
        if matches!(id, "vscode" | "claude_code") {
            assert_eq!(value[key]["trustd"]["type"], "stdio");
        }
        if id == "gemini" {
            assert_eq!(value[key]["trustd"]["timeout"], 3_600_000);
        }
    }
}

#[test]
fn toml_preserves_comments_and_other_tables() {
    let input = "# personal settings\nmodel = 'chosen'\n[mcp_servers.other]\ncommand = 'keep'\n";
    let output = render(input, "codex", &command(), endpoint(), false).unwrap();
    assert!(output.starts_with(input));
    let doc: toml::Value = toml::from_str(&output).unwrap();
    assert_eq!(
        doc["mcp_servers"]["other"]["command"].as_str(),
        Some("keep")
    );
    assert_eq!(
        doc["mcp_servers"]["trustd"]["tool_timeout_sec"].as_integer(),
        Some(3600)
    );
    let inline = render(
        "mcp_servers = { other = {command = 'keep'} }",
        "codex",
        &command(),
        endpoint(),
        false,
    )
    .unwrap();
    assert!(toml::from_str::<toml::Value>(&inline).is_ok());
}

#[test]
fn replacement_is_explicit_and_removes_obsolete_transport_fields() {
    for (id, source) in [
        (
            "codex",
            "[mcp_servers.trustd]\nurl = 'http://old/mcp'\nenv = {TOKEN = 'private'}",
        ),
        (
            "cursor",
            r#"{"mcpServers":{"trustd":{"url":"http://old/mcp","env":{"TOKEN":"private"}},"other":{}}}"#,
        ),
    ] {
        assert!(render(source, id, &command(), endpoint(), false).is_err());
        let updated = render(source, id, &command(), endpoint(), true).unwrap();
        assert!(!updated.contains("private"));
        assert!(!updated.contains("http://old"));
        assert_eq!(
            updated,
            render(&updated, id, &command(), endpoint(), true).unwrap()
        );
    }
}

#[test]
fn malformed_duplicate_or_wrong_type_configs_are_not_replaced() {
    for source in [
        "[]",
        "null",
        "{",
        r#"{"mcpServers":[]}"#,
        r#"{"mcpServers":{},"mcpServers":{}}"#,
        r#"{"mcpServers":{"trustd":{},"trustd":{}}}"#,
        r#"{"missing":1 "comma":2}"#,
    ] {
        assert!(
            render(source, "cursor", &command(), endpoint(), true).is_err(),
            "{source}"
        );
    }
    for source in ["mcp_servers = 1", "invalid toml"] {
        assert!(render(source, "codex", &command(), endpoint(), true).is_err());
    }
}

#[test]
fn writes_are_backed_up_and_failures_do_not_stop_other_clients() {
    let dir = tempfile::tempdir().unwrap();
    let agents: Vec<_> = [
        ("codex", "Codex"),
        ("cursor", "Cursor"),
        ("gemini", "Gemini"),
    ]
    .into_iter()
    .map(|(id, name)| Agent {
        id,
        name,
        path: dir.path().join(id).join("config"),
        detected: false,
    })
    .collect();
    fs::create_dir_all(agents[0].path.parent().unwrap()).unwrap();
    fs::write(&agents[0].path, "invalid = ").unwrap();
    fs::create_dir_all(agents[1].path.parent().unwrap()).unwrap();
    let original = "{\"preferences\":{\"key\":\"keep\"}}";
    fs::write(&agents[1].path, original).unwrap();
    let ids = agents.iter().map(|a| a.id.to_owned()).collect::<Vec<_>>();
    let results = configure(&agents, &ids, &command(), endpoint(), false);
    assert!(results[0].error.is_some());
    assert_eq!(fs::read_to_string(&agents[0].path).unwrap(), "invalid = ");
    assert!(results[1].error.is_none());
    assert_eq!(
        fs::read_to_string(results[1].backup.as_ref().unwrap()).unwrap(),
        original
    );
    assert!(results[2].error.is_none());
    assert!(results[2].backup.is_none());
    let results = configure(&agents, &[], &command(), endpoint(), true);
    assert!(results.is_empty());
    assert!(validate(&["arbitrary".into()], &agents).is_err());
    assert!(validate(&["codex".into(), "codex".into()], &agents).is_err());
}

#[test]
fn paths_with_spaces_quotes_and_unicode_are_data_not_shell_commands() {
    let path = PathBuf::from("/安装/space ' quote\" $name/trustd-stdio");
    for id in ["cursor", "codex"] {
        let output = render("", id, &path, endpoint(), false).unwrap();
        if id == "codex" {
            let doc: toml::Value = toml::from_str(&output).unwrap();
            assert_eq!(
                doc["mcp_servers"]["trustd"]["command"].as_str(),
                path.to_str()
            );
        } else {
            assert_eq!(
                json(&output)["mcpServers"]["trustd"]["command"],
                path.to_str().unwrap()
            );
        }
    }
}

#[test]
fn legacy_commands_migrate_once_without_touching_custom_settings() {
    let dir = tempfile::tempdir().unwrap();
    let stable = dir.path().join(crate::bundle::filename("trustd-stdio"));
    fs::write(&stable, b"fixture").unwrap();
    let old = dir
        .path()
        .join("versions")
        .join("a".repeat(64))
        .join(stable.file_name().unwrap());
    for id in ["codex", "cursor", "vscode"] {
        let agent = Agent {
            id,
            name: id,
            path: dir.path().join(format!("{id}.config")),
            detected: true,
        };
        let source = if id == "codex" {
            format!(
                "# keep\n[mcp_servers.trustd]\ncommand = '{}' # path comment\nargs = ['--endpoint', '127.0.0.1:45678']\nenv = {{KEEP = 'value'}}\ntool_timeout_sec = 999\n",
                old.display()
            )
        } else {
            let key = if id == "vscode" {
                "servers"
            } else {
                "mcpServers"
            };
            format!(
                "// keep\n{}",
                serde_json::json!({key: {"trustd": {"command": old, "args": ["custom"], "env": {"KEEP": "value"}, "timeout": 999}}})
            )
        };
        fs::write(&agent.path, &source).unwrap();
        let results = configure(
            std::slice::from_ref(&agent),
            &[],
            &stable,
            endpoint(),
            false,
        );
        assert_eq!(results.len(), 1);
        assert!(results[0].error.is_none(), "{:?}", results[0].error);
        assert_eq!(
            fs::read_to_string(results[0].backup.as_ref().unwrap()).unwrap(),
            source
        );
        let migrated = fs::read_to_string(&agent.path).unwrap();
        assert!(migrated.contains("keep"));
        if id == "codex" {
            assert!(migrated.contains("# path comment"));
            let mut before: toml::Value = toml::from_str(&source).unwrap();
            before["mcp_servers"]["trustd"]["command"] =
                toml::Value::String(stable.to_str().unwrap().into());
            assert_eq!(before, toml::from_str::<toml::Value>(&migrated).unwrap());
        } else {
            let key = if id == "vscode" {
                "servers"
            } else {
                "mcpServers"
            };
            let mut before = json(&source);
            before[key]["trustd"]["command"] = stable.to_str().unwrap().into();
            assert_eq!(before, json(&migrated));
        }
        let results = configure(
            std::slice::from_ref(&agent),
            &[id.into()],
            &stable,
            endpoint(),
            false,
        );
        assert!(results[0].error.is_none());
        assert!(results[0].backup.is_none());
        assert_eq!(fs::read_to_string(&agent.path).unwrap(), migrated);
    }
}

#[test]
fn custom_and_remote_commands_are_not_implicitly_migrated() {
    let stable = PathBuf::from("/fixed/trustd-stdio");
    for old in [
        "/other/versions/aaaaaaaa/trustd-stdio",
        "/fixed/versions/../trustd-stdio",
        "/fixed/versions/not-a-digest/trustd-stdio",
    ] {
        let source = serde_json::json!({"mcpServers": {"trustd": {"command": old}}}).to_string();
        assert!(stable_entry(&source, "cursor", &stable).unwrap().is_none());
    }
    let source = serde_json::json!({"mcpServers": {"trustd": {"command": stable, "url": "https://custom/mcp"}}}).to_string();
    assert!(stable_entry(&source, "cursor", &stable).unwrap().is_none());
}
