use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "plexy-glass", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Attach to (or start) the daemon and open a session.
    Attach,
    /// Run the daemon (used internally by auto-spawn; `--foreground` for dev).
    Daemon(plexy_glass_daemon::DaemonArgs),
    /// Stop the running daemon (`SIGTERM`, then `SIGKILL` after a grace period).
    Kill,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Attach) {
        Command::Attach => {
            plexy_glass_client::run(plexy_glass_client::ClientArgs {}).await?;
        }
        Command::Daemon(args) => {
            plexy_glass_daemon::run(args).await?;
        }
        Command::Kill => match plexy_glass_client::kill().await? {
            plexy_glass_client::KillOutcome::NoDaemon => println!("no daemon running"),
            plexy_glass_client::KillOutcome::Stopped => println!("daemon stopped"),
            plexy_glass_client::KillOutcome::ForceKilled => {
                println!("daemon did not respond to SIGTERM; sent SIGKILL")
            }
        },
    }
    Ok(())
}
