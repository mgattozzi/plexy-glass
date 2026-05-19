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
        Cmd::New { name: _, cmd: _, args: _ } => {
            // Task 16 implements this properly.
            eprintln!("error: 'new' subcommand not yet implemented");
            std::process::exit(1);
        }
        Cmd::Attach { name: _ } => {
            // Existing attach behavior: connect to (or auto-spawn) the daemon
            // and open a session. Task 16/17 will thread the `name` argument
            // through once multi-session support is wired up.
            plexy_glass_client::run(plexy_glass_client::ClientArgs {}).await?;
        }
        Cmd::List => {
            // Task 17 implements this properly.
            eprintln!("error: 'list' subcommand not yet implemented");
            std::process::exit(1);
        }
        Cmd::Kill { name } => match name {
            Some(_session_name) => {
                // Task 16 implements per-session kill.
                eprintln!("error: 'kill -n NAME' not yet implemented (Task 16)");
                std::process::exit(1);
            }
            None => {
                // Existing behavior: kill the daemon.
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
