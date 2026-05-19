use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "plexy-glass", about = "A terminal multiplexer with first-class OSC handling", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Attach to a session (creates it if it doesn't exist).
    Attach {
        /// Session name. If omitted: attach to the only existing session, or create "main" if none.
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
    },
    /// List all sessions.
    List,
    /// Kill a single session by name, or the daemon if no -n is given.
    Kill {
        /// Session name to kill. If omitted, kills the daemon.
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
    },
    /// Start the daemon (used internally by auto-spawn; `--foreground` for dev).
    Daemon(plexy_glass_daemon::DaemonArgs),
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
    // Default to `attach` with no name when no subcommand is given.
    match cli.command.unwrap_or(Cmd::Attach { name: None }) {
        Cmd::Attach { name } => {
            plexy_glass_client::client_attach_smart(name).await?;
        }
        Cmd::List => {
            plexy_glass_client::client_list().await?;
        }
        Cmd::Kill { name } => match name {
            Some(session_name) => {
                plexy_glass_client::client_kill_session(session_name).await?;
            }
            None => {
                // Existing behaviour: kill the daemon.
                match plexy_glass_client::kill().await? {
                    plexy_glass_client::KillOutcome::NoDaemon => println!("no daemon running"),
                    plexy_glass_client::KillOutcome::Stopped { count } => {
                        let plural = if count == 1 { "" } else { "s" };
                        println!("stopped {count} daemon{plural}");
                    }
                    plexy_glass_client::KillOutcome::ForceKilled { count } => {
                        let plural = if count == 1 { "" } else { "s" };
                        println!(
                            "force-killed {count} daemon{plural} (SIGTERM ignored, sent SIGKILL)"
                        );
                    }
                }
            }
        },
        Cmd::Daemon(args) => {
            plexy_glass_daemon::run(args).await?;
        }
    }
    Ok(())
}
