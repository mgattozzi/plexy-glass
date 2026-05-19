use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "plexy-glass", about = "A terminal multiplexer with first-class OSC handling", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Create a new session and attach to it.
    New {
        /// Session name.
        #[arg(short = 'n', long = "name")]
        name: String,
        /// Command to run in the new session.
        #[arg(short = 'c', long = "cmd")]
        cmd: Option<String>,
        /// Arguments to pass to the command.
        #[arg(long = "args", num_args = 0..)]
        args: Vec<String>,
    },
    /// Attach to an existing session (or start the daemon and open a session).
    Attach {
        /// Session name to attach to.
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
        Cmd::New { name, cmd, args } => {
            plexy_glass_client::client_new(name, cmd, args).await?;
        }
        Cmd::Attach { name } => {
            // When no name is given, `create_if_missing=true` preserves the
            // existing default-session behaviour that the e2e tests rely on.
            // When a specific name is given, require the session to exist
            // (`create_if_missing=false`); Task 17 will add the smart default.
            let create = name.is_none();
            plexy_glass_client::run(name, create, None).await?;
        }
        Cmd::List => {
            // Task 17 implements this properly.
            eprintln!("error: 'list' subcommand not yet implemented");
            std::process::exit(1);
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
