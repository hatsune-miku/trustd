use anyhow::Result;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(name="trustd-agent-stdio",version=localshelld::VERSION,about="Scoped MCP adapter for a trustd-owned Codex conversation")]
struct Options {
    #[arg(long)]
    endpoint: SocketAddr,
    #[arg(long)]
    token_file: PathBuf,
}

fn main() -> Result<()> {
    let options = Options::parse();
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(localshelld::remote::agent_stdio::serve(
        options.endpoint,
        &options.token_file,
    ));
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    result
}
