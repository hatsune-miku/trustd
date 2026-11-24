//! Placeholder that release packages keep while devices update from versions
//! that still shipped this program: their `trustd update` checks every
//! program it knows, including this one, by its `--version`. It is never
//! installed, and installation removes an earlier copy.
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("trustd-agent-stdio {}", localshelld::VERSION);
        return;
    }
    eprintln!("trustd-agent-stdio is no longer used; remote Codex conversations were removed.");
    std::process::exit(2);
}
