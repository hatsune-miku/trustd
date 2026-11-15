fn main() {
    println!("cargo:rerun-if-env-changed=LOCALSHELLD_RELEASE_TAG");
    let version = std::env::var("LOCALSHELLD_RELEASE_TAG")
        .map(|v| v.trim_start_matches('v').to_owned())
        .unwrap_or_else(|_| format!("{}-dev", env!("CARGO_PKG_VERSION")));
    println!("cargo:rustc-env=TRUSTD_REMOTE_VERSION={version}");
}
