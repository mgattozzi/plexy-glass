use std::process;

use clap::{Parser, Subcommand};

// Compiled only for tests: exercises the version formatter that `build.rs`
// shares via `#[path]`. Not present in release builds (no dead code).
#[cfg(test)]
mod version_fmt;

#[derive(Debug, Parser)]
#[command(
    name = "plexy-glass",
    about = "A terminal multiplexer with first-class OSC handling",
    version = env!("PLEXY_GLASS_VERSION")
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommands>,

    /// Run against a daemon on a remote host over SSH (an ssh_config alias or
    /// user@host). Applies to any connection verb.
    #[arg(short = 'H', long = "host", global = true)]
    host: Option<String>,
    /// Path to `plexy-glass` on the remote (default: found on PATH, or the
    /// --install cache path).
    #[arg(long = "remote-bin", global = true)]
    remote_bin: Option<String>,
    /// Provision the remote binary from the nightly release before connecting.
    #[arg(long = "install", global = true)]
    install: bool,
}

#[derive(Debug, Subcommand)]
enum Subcommands {
    /// Attach to a session (creates it if it doesn't exist).
    Attach {
        /// Session name (default: "main").
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
    },
    /// List all sessions.
    List,
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
    /// Reload the daemon's config from the platform config dir (config.kdl).
    Reload,
    /// Run command-prompt lines against a session (see `docs/configuration.md` §6).
    Cmd {
        /// Target session (defaults to the sole running session).
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
        /// One or more prompt lines, e.g. "split v" "layout tiled".
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        lines: Vec<String>,
    },
    /// Type text into a session's focused pane (popup-aware).
    Send {
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
        /// Append Enter (a carriage return) after the text.
        #[arg(long = "enter")]
        enter: bool,
        /// Text fragments, joined with single spaces.
        #[arg(
            required_unless_present = "enter",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        text: Vec<String>,
    },
    /// Print the focused pane's visible screen text (popup-aware).
    ///
    /// With --last-command, prints the output of the last completed OSC 133
    /// command block (scrollback-inclusive) instead of the full screen.
    /// Exits 1 when no completed block exists (shell integration not active).
    Capture {
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
        /// Capture the last completed command block's output (requires shell
        /// integration via OSC 133).
        #[arg(long = "last-command")]
        last_command: bool,
        /// Print one JSON object {"output", "exit_code", "command_line"}
        /// instead of plain text (only with --last-command).
        #[arg(long = "json", requires = "last_command")]
        json: bool,
    },
    /// Run a command in the focused pane and wait for it to finish (requires
    /// OSC 133 shell integration).
    Run {
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
        /// Give up after SECS seconds (exit 124; the command keeps running).
        #[arg(long = "timeout", value_name = "SECS")]
        timeout: Option<u64>,
        /// Print one JSON object {"output", "exit_code", "timed_out",
        /// "command_line"} instead of plain output (exit codes unchanged).
        #[arg(long = "json")]
        json: bool,
        /// Command text fragments, joined with single spaces.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Print an OSC 133 shell-integration snippet for your shell, then exit.
    ///
    /// Add it to your shell's rc file to light up the command-block features
    /// (exit-status borders, prompt nav, block mode, history output search,
    /// `run`, completion toasts), e.g. eval "$(plexy-glass shell-integration zsh)".
    ShellIntegration {
        /// One of: bash, zsh, fish, nu.
        shell: String,
    },
    /// Start the daemon (used internally by auto-spawn; `--foreground` for dev).
    Daemon(plexy_glass_daemon::DaemonArgs),
    /// Relay stdio to the local daemon's socket (used by `-H` over SSH; run on
    /// the remote host). Not typically invoked by hand.
    Bridge {
        /// Connect only; do not spawn a daemon if none is running.
        #[arg(long = "no-spawn")]
        no_spawn: bool,
    },
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
    let install = if cli.install {
        plexy_glass_client::InstallPolicy::Provision
    } else {
        plexy_glass_client::InstallPolicy::UseExisting
    };
    let target = plexy_glass_client::Target {
        // The CLI boundary: `-H` is a plain string, and its absence means the
        // LOCAL daemon — a destination, not a missing value. Parse both into
        // `Host` here so nothing downstream re-derives "local" from a `None`.
        host: cli.host.map_or(plexy_glass_client::Host::Local, |h| {
            plexy_glass_client::Host::Remote(plexy_glass_client::RemoteName::from(h))
        }),
        remote_bin: cli.remote_bin,
        install,
    };
    // Default to `attach` with no name when no subcommand is given.
    match cli.command.unwrap_or(Subcommands::Attach { name: None }) {
        Subcommands::Attach { name } => {
            plexy_glass_client::client_attach_smart(&target, name).await?;
        }
        Subcommands::List => {
            plexy_glass_client::client_list(&target).await?;
        }
        Subcommands::Kill { name, all } => {
            if let Some(session_name) = name {
                plexy_glass_client::client_kill_session(&target, session_name).await?;
            } else if target.host.is_remote() {
                // No `-n`, remote: `kill` signals a process, not a daemon-protocol
                // request, so it must run ON the remote (a local kill would stop
                // THIS machine's daemon). Runs `<remote-bin> kill [--all]` over SSH.
                plexy_glass_client::client_kill_remote(&target, all).await?;
            } else {
                // No `-n`, local: stop the daemon. Default scopes to this runtime
                // dir's daemon; `--all` sweeps every daemon for the user.
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
        }
        Subcommands::Reload => {
            plexy_glass_client::client_reload_config(&target).await?;
        }
        Subcommands::Cmd { name, lines } => {
            match plexy_glass_client::client_run_commands(&target, name, lines).await {
                Ok(true) => {}
                Ok(false) => process::exit(1),
                Err(e) => {
                    eprintln!("plexy-glass: {e}");
                    process::exit(1);
                }
            }
        }
        Subcommands::Send { name, enter, text } => {
            let mut bytes = text.join(" ").into_bytes();
            if enter {
                bytes.push(b'\r');
            }
            match plexy_glass_client::client_send_input(&target, name, bytes).await {
                Ok(true) => {}
                Ok(false) => process::exit(1),
                Err(e) => {
                    eprintln!("plexy-glass: {e}");
                    process::exit(1);
                }
            }
        }
        Subcommands::Capture {
            name,
            last_command,
            json,
        } => {
            let result = if json {
                plexy_glass_client::client_capture_block(&target, name).await
            } else {
                plexy_glass_client::client_capture(&target, name, last_command).await
            };
            match result {
                Ok(true) => {}
                Ok(false) => process::exit(1),
                Err(e) => {
                    eprintln!("plexy-glass: {e}");
                    process::exit(1);
                }
            }
        }
        Subcommands::Run {
            name,
            timeout,
            json,
            text,
        } => {
            match plexy_glass_client::client_exec(&target, name, text.join(" "), timeout, json)
                .await
            {
                // 0 falls through to the normal `Ok(())` return; any other code
                // (command exit passthrough, 124 timeout, 1 refusal) exits now.
                Ok(0) => {}
                Ok(code) => process::exit(code),
                Err(e) => {
                    eprintln!("plexy-glass: {e}");
                    process::exit(1);
                }
            }
        }
        Subcommands::ShellIntegration { shell } => {
            if let Some(snippet) = plexy_glass_client::shell_integration_snippet(&shell) {
                print!("{snippet}");
            } else {
                eprintln!("plexy-glass: unknown shell {shell:?} (try bash, zsh, fish, or nu)");
                process::exit(1);
            }
        }
        Subcommands::Daemon(args) => {
            plexy_glass_daemon::run(args).await?;
        }
        Subcommands::Bridge { no_spawn } => {
            let connect = if no_spawn {
                plexy_glass_client::Connect::Only
            } else {
                plexy_glass_client::Connect::Spawn
            };
            plexy_glass_client::run_bridge(connect).await?;
        }
    }
    Ok(())
}
