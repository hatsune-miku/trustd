use std::{fs, path::Path, process::Command};
use trustd_remote::{
    Identity,
    relay::{Config, token_hash},
};

fn run(config: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_trustd-relay"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn positional_identity_defaults_and_legacy_overrides_share_the_registry() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("registry.json");
    let identity = Identity::generate().unwrap();
    let id = identity.id().unwrap();
    run(&config, &["add-device", &identity.public_key]);
    let token = fs::read_to_string(directory.path().join(format!("{id}.token"))).unwrap();
    let registry: Config = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(registry.devices[&id], token_hash(&token));

    let custom_token = directory.path().join("custom.token");
    run(
        &config,
        &[
            "add-device",
            "--device",
            &id.to_uppercase(),
            "--token-file",
            custom_token.to_str().unwrap(),
        ],
    );
    let replaced = fs::read_to_string(custom_token).unwrap();
    let registry: Config = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_ne!(token, replaced);
    assert_eq!(registry.devices.len(), 1);
    assert_eq!(registry.devices[&id], token_hash(&replaced));

    run(&config, &["remove-device", &identity.public_key]);
    let registry: Config = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
    assert!(registry.devices.is_empty());
}
