use clap::Parser;
use rotten_app::cli::{Cli, Commands};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if matches!(cli.command, Some(Commands::Probe)) {
        println!("cermin probe ok");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("cermin=info".parse()?))
        .init();

    // Single-threaded runtime: multi-thread tokio has hung on some Windows-gnu builds.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(cli.run());
    // A cancelled interactive PIN read can still be blocked on stdin. It must
    // not hold the process open after cooperative session/audio cleanup.
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}
