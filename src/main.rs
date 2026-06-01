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
    /// List sessions saved on disk (running or not).
    ListSaved,
    /// Kill a single session by name, or this runtime dir's daemon if no -n is
    /// given. With --all, stop every plexy-glass daemon for the current user.
    Kill {
        /// Session name to kill. If omitted, kills the daemon.
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
        /// Stop every `plexy-glass` daemon owned by the current user, across all
        /// runtime dirs (orphan cleanup). Ignored when `-n` is given.
        #[arg(long = "all", conflicts_with = "name")]
        all: bool,
    },
    /// Reload the daemon's config from ~/.config/plexy-glass/config.kdl.
    Reload,
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
        Cmd::ListSaved => {
            plexy_glass_client::client_list_saved().await?;
        }
        Cmd::Kill { name, all } => match name {
            Some(session_name) => {
                plexy_glass_client::client_kill_session(session_name).await?;
            }
            None => {
                // No `-n`: stop the daemon. Default scopes to this runtime dir's
                // daemon; `--all` sweeps every daemon for the user.
                let outcome = if all {
                    plexy_glass_client::kill_all().await?
                } else {
                    plexy_glass_client::kill().await?
                };
                match outcome {
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
        Cmd::Reload => {
            plexy_glass_client::client_reload_config().await?;
        }
        Cmd::Daemon(args) => {
            plexy_glass_daemon::run(args).await?;
        }
    }
    Ok(())
}
