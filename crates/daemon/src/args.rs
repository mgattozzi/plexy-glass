use clap::Parser;

#[derive(Debug, Clone, Parser)]
pub struct DaemonArgs {
    /// Stay in the foreground and log to stdout/stderr (for development).
    #[arg(long)]
    pub foreground: bool,
}
