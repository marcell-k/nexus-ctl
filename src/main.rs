mod commands;

use std::process::exit;

use clap::Parser;
use commands::{
    cmd_kill, cmd_logs_pull, cmd_logs_tail, cmd_restart, cmd_shell, cmd_start, cmd_status,
    cmd_stop, Commands, LogsAction,
};

const DEFAULT_HOST: &str = "Marci@windows.tail0212d0.ts.net";
const DEFAULT_REMOTE_PROJECT: &str = r"C:\Users\Marci\Desktop\nexus-trade";
const DEFAULT_TASK_PREFIX: &str = "nexus-trade";

pub fn host() -> String {
    std::env::var("NEXUS_CTL_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string())
}

pub fn remote_project() -> String {
    std::env::var("NEXUS_CTL_REMOTE_PROJECT").unwrap_or_else(|_| DEFAULT_REMOTE_PROJECT.to_string())
}

pub fn task_prefix() -> String {
    std::env::var("NEXUS_CTL_TASK_PREFIX").unwrap_or_else(|_| DEFAULT_TASK_PREFIX.to_string())
}

fn load_dotenv() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let path = std::path::Path::new(&home).join(".config/nexus-ctl/.env");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut value = value.trim();
        if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
            || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
        {
            value = &value[1..value.len() - 1];
        }
        if std::env::var(key).is_err() {
            std::env::set_var(key, value);
        }
    }
}

#[derive(Parser)]
#[command(name = "nexus-ctl", version, about, long_about = None)]
struct Cli {
    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Commands,
}

fn main() {
    load_dotenv();
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Start { env } => cmd_start(env, cli.dry_run),
        Commands::Stop { env, grace, force } => cmd_stop(env, grace, force, cli.dry_run),
        Commands::Restart { env, grace } => cmd_restart(env, grace, cli.dry_run),
        Commands::Status => cmd_status(cli.dry_run),
        Commands::Kill => cmd_kill(cli.dry_run),
        Commands::Logs { action } => match action {
            LogsAction::Tail { env, lines } => cmd_logs_tail(env, lines, cli.dry_run),
            LogsAction::Pull { env, dest, rsync } => cmd_logs_pull(env, dest, rsync, cli.dry_run),
        },
        Commands::Shell => cmd_shell(cli.dry_run),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        exit(1);
    }
}
