use std::process::{exit, Command};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

const DEFAULT_HOST: &str = "Marci@windows.tail0212d0.ts.net";
const DEFAULT_REMOTE_PROJECT: &str = r"C:\Users\Marci\Desktop\nexus-trade";
const DEFAULT_TASK_PREFIX: &str = "nexus-trade";

fn host() -> String {
    std::env::var("NEXUS_CTL_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string())
}

fn remote_project() -> String {
    std::env::var("NEXUS_CTL_REMOTE_PROJECT").unwrap_or_else(|_| DEFAULT_REMOTE_PROJECT.to_string())
}

fn task_prefix() -> String {
    std::env::var("NEXUS_CTL_TASK_PREFIX").unwrap_or_else(|_| DEFAULT_TASK_PREFIX.to_string())
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
enum Env {
    Ictrading,
    Ftmo,
    All,
}

impl Env {
    fn instances(self) -> Vec<&'static str> {
        match self {
            Env::Ictrading => vec!["ictrading"],
            Env::Ftmo => vec!["ftmo"],
            Env::All => vec!["ictrading", "ftmo"],
        }
    }
}

fn task_name(instance: &str) -> String {
    format!("{}-{}", task_prefix(), instance)
}

fn log_path(instance: &str) -> String {
    format!(r"{}\logs\{}.log", remote_project(), instance)
}

#[derive(Parser)]
#[command(name = "nexus-ctl", version, about, long_about = None)]
struct Cli {
    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Start {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
    },

    Stop {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
        #[arg(long)]
        no_verify: bool,
        #[arg(long)]
        force: bool,
    },

    Restart {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
    },

    Status,

    Kill,

    Logs {
        #[command(subcommand)]
        action: LogsAction,
    },

    Shell,
}

#[derive(Subcommand)]
enum LogsAction {
    Tail {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: u32,
    },

    Pull {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
        #[arg(long, default_value = "~/Desktop/nexus-trade-logs")]
        dest: String,
        #[arg(long)]
        rsync: bool,
    },
}

fn run_remote(ps_command: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("[dry-run] ssh {} -- {}", host(), ps_command);
        return Ok(());
    }

    let status = Command::new("ssh")
        .arg("-t")
        .arg(host())
        .arg(ps_command)
        .status()
        .context("failed to spawn `ssh` — is it installed and on your PATH?")?;

    if !status.success() {
        bail!(
            "remote command exited with status {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
    }
    Ok(())
}

fn ps(cmd: impl Into<String>) -> String {
    format!(
        "powershell -NoProfile -Command \"{}\"",
        cmd.into().replace('"', "`\"")
    )
}

fn run_local(program: &str, args: &[String], dry_run: bool) -> Result<()> {
    if dry_run {
        println!("[dry-run] {} {}", program, args.join(" "));
        return Ok(());
    }

    let status = Command::new(program).args(args).status().with_context(|| {
        format!("failed to spawn `{program}` — is it installed and on your PATH?")
    })?;

    if !status.success() {
        bail!("{} exited with status {:?}", program, status.code());
    }
    Ok(())
}

fn cmd_start(env: Env, dry_run: bool) -> Result<()> {
    for inst in env.instances() {
        println!("==> starting {inst}");
        run_remote(
            &ps(format!(
                "Start-ScheduledTask -TaskName '{}'",
                task_name(inst)
            )),
            dry_run,
        )?;
    }
    Ok(())
}

fn cmd_stop(env: Env, no_verify: bool, force: bool, dry_run: bool) -> Result<()> {
    for inst in env.instances() {
        println!("==> stopping {inst}");
        run_remote(
            &ps(format!(
                "Stop-ScheduledTask -TaskName '{}'",
                task_name(inst)
            )),
            dry_run,
        )?;
    }

    if no_verify {
        return Ok(());
    }

    println!("==> checking for lingering uv.exe process(es)");
    let check = ps("Get-Process uv -ErrorAction SilentlyContinue | Select-Object Id, StartTime");
    let _ = run_remote(&check, dry_run);

    if force {
        println!("==> force-killing any remaining uv.exe process(es)");
        run_remote(
            &ps("Get-Process uv -ErrorAction SilentlyContinue | Stop-Process -Force"),
            dry_run,
        )?;
    } else {
        println!(
            "(if a process is still listed above, re-run with `--force`, or `nexus-ctl kill`)"
        );
    }
    Ok(())
}

fn cmd_restart(env: Env, dry_run: bool) -> Result<()> {
    cmd_stop(env, false, true, dry_run)?;
    cmd_start(env, dry_run)
}

fn cmd_status(dry_run: bool) -> Result<()> {
    let mut cmd_parts = Vec::new();
    for inst in Env::All.instances() {
        cmd_parts.push(format!(
            "Get-ScheduledTaskInfo -TaskName '{}'",
            task_name(inst)
        ));
    }
    cmd_parts.push(
        "Get-Process uv -ErrorAction SilentlyContinue | Select-Object Id, StartTime".to_string(),
    );
    run_remote(&ps(cmd_parts.join("; ")), dry_run)
}

fn cmd_kill(dry_run: bool) -> Result<()> {
    run_remote(
        &ps("Get-Process uv -ErrorAction SilentlyContinue | Stop-Process -Force"),
        dry_run,
    )
}

fn cmd_logs_tail(env: Env, lines: u32, dry_run: bool) -> Result<()> {
    let insts = env.instances();
    if insts.len() > 1 {
        bail!("`logs tail` only supports one instance at a time — pass `ictrading` or `ftmo`");
    }
    let inst = insts[0];
    println!("==> tailing {inst} (Ctrl+C to stop; the task keeps running)");
    run_remote(
        &ps(format!(
            "Get-Content '{}' -Wait -Tail {}",
            log_path(inst),
            lines
        )),
        dry_run,
    )
}

fn cmd_logs_pull(env: Env, dest: String, use_rsync: bool, dry_run: bool) -> Result<()> {
    let dest = shellexpand_home(&dest);
    if !dry_run {
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("failed to create destination dir {dest}"))?;
    }

    if use_rsync {
        let remote = format!("{}:{}/logs/", host(), to_forward_slashes(&remote_project()));
        println!("==> rsync logs -> {dest}");
        run_local(
            "rsync",
            &["-avz".to_string(), remote, format!("{dest}/")],
            dry_run,
        )
    } else {
        for inst in env.instances() {
            let remote = format!(
                "{}:{}/logs/{}.log",
                host(),
                to_forward_slashes(&remote_project()),
                inst
            );
            println!("==> scp {inst}.log -> {dest}");
            run_local("scp", &[remote, format!("{dest}/")], dry_run)?;
        }
        Ok(())
    }
}

fn cmd_shell(dry_run: bool) -> Result<()> {
    if dry_run {
        println!("[dry-run] ssh {}", host());
        return Ok(());
    }
    let status = Command::new("ssh")
        .arg(host())
        .status()
        .context("failed to spawn `ssh`")?;
    if !status.success() {
        exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn to_forward_slashes(windows_path: &str) -> String {
    windows_path.replace('\\', "/")
}

fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
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

fn main() {
    load_dotenv();
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Start { env } => cmd_start(env, cli.dry_run),
        Commands::Stop {
            env,
            no_verify,
            force,
        } => cmd_stop(env, no_verify, force, cli.dry_run),
        Commands::Restart { env } => cmd_restart(env, cli.dry_run),
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
