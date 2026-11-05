use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "trustd-user-headless", version = localshelld::VERSION, about = "trustd current-user daemon without desktop authorization")]
struct Options {
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    let options = Options::parse();
    localshelld::daemon::user(options.config, true)
}
