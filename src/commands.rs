use std::io::Write;
use std::process::{exit, Command, Stdio};

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};

const GRACEFUL_STOP_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'

$proc = Get-CimInstance Win32_Process -Filter "Name='uv.exe'" |
    Where-Object { $_.CommandLine -like "*$EnvFilter*" } |
    Select-Object -First 1

if (-not $proc) {
    Write-Host "[$Instance] no running uv.exe process found (already stopped?)"
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    exit 0
}

$targetPid = $proc.ProcessId
Write-Host "[$Instance] PID $targetPid -- sending graceful stop signal (same as Ctrl+C)"

Add-Type -Name NexusCtrlC -Namespace NexusCtl -MemberDefinition @"
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
public static extern bool AttachConsole(uint dwProcessId);
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
public static extern bool FreeConsole();
[System.Runtime.InteropServices.DllImport("kernel32.dll")]
public static extern bool SetConsoleCtrlHandler(IntPtr HandlerRoutine, bool Add);
[System.Runtime.InteropServices.DllImport("kernel32.dll")]
public static extern bool GenerateConsoleCtrlEvent(uint dwCtrlEvent, uint dwProcessGroupId);
"@

[NexusCtl.NexusCtrlC]::FreeConsole() | Out-Null
[NexusCtl.NexusCtrlC]::AttachConsole([uint32]$targetPid) | Out-Null
[NexusCtl.NexusCtrlC]::SetConsoleCtrlHandler([IntPtr]::Zero, $true) | Out-Null
[NexusCtl.NexusCtrlC]::GenerateConsoleCtrlEvent(0, 0) | Out-Null
Start-Sleep -Milliseconds 500
[NexusCtl.NexusCtrlC]::FreeConsole() | Out-Null

Write-Host "[$Instance] waiting up to $GraceSeconds second(s) for a clean shutdown..."
$deadline = (Get-Date).AddSeconds($GraceSeconds)
while ((Get-Date) -lt $deadline) {
    if (-not (Get-Process -Id $targetPid -ErrorAction SilentlyContinue)) {
        Write-Host "[$Instance] exited cleanly."
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        exit 0
    }
    Start-Sleep -Seconds 2
}

Write-Host "[$Instance] WARNING: still running after $GraceSeconds second(s) -- force-killing now."
Write-Host "[$Instance] WARNING: it did not get to run its own shutdown logic -- verify open positions manually."
Stop-Process -Id $targetPid -Force -ErrorAction SilentlyContinue
Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
"#;

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum Env {
    Ictrading,
    Ftmo,
    All,
}

impl Env {
    pub fn instances(self) -> Vec<&'static str> {
        match self {
            Env::Ictrading => vec!["ictrading"],
            Env::Ftmo => vec!["ftmo"],
            Env::All => vec!["ictrading", "ftmo"],
        }
    }
}

#[derive(Subcommand)]
pub enum Commands {
    Start {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
    },

    Stop {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,

        #[arg(long, default_value_t = 60)]
        grace: u32,

        #[arg(long)]
        force: bool,
    },

    Restart {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
        #[arg(long, default_value_t = 60)]
        grace: u32,
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
pub enum LogsAction {
    Tail {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: u32,
    },

    Pull {
        #[arg(value_enum, default_value_t = Env::All)]
        env: Env,
        /// Local destination directory (created if missing).
        #[arg(long, default_value = "~/Desktop/nexus-trade-logs")]
        dest: String,
        /// Use rsync (incremental) instead of scp (full copy each time).
        #[arg(long)]
        rsync: bool,
    },
}

pub fn cmd_start(env: Env, dry_run: bool) -> Result<()> {
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

pub fn cmd_stop(env: Env, grace: u32, force: bool, dry_run: bool) -> Result<()> {
    for inst in env.instances() {
        if force {
            println!("==> force-stopping {inst} immediately (no grace period — it will NOT close positions)");
            run_remote(
                &ps(format!(
                    "Stop-ScheduledTask -TaskName '{}'; \
                     Get-CimInstance Win32_Process -Filter \"Name='uv.exe'\" | \
                     Where-Object {{ $_.CommandLine -like '*.env.{}*' }} | \
                     ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}",
                    task_name(inst),
                    inst
                )),
                dry_run,
            )?;
            continue;
        }

        println!("==> stopping {inst} gracefully (grace period: {grace}s)");
        let header = format!(
            "$Instance = '{}'\n$EnvFilter = '{}'\n$GraceSeconds = {}\n$TaskName = '{}'\n",
            ps_single_quote(inst),
            ps_single_quote(&format!(".env.{inst}")),
            grace,
            ps_single_quote(&task_name(inst)),
        );
        run_remote_script(&format!("{header}{GRACEFUL_STOP_SCRIPT}"), dry_run)?;
    }
    Ok(())
}

pub fn cmd_restart(env: Env, grace: u32, dry_run: bool) -> Result<()> {
    cmd_stop(env, grace, false, dry_run)?;
    cmd_start(env, dry_run)
}

pub fn cmd_status(dry_run: bool) -> Result<()> {
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

pub fn cmd_kill(dry_run: bool) -> Result<()> {
    run_remote(
        &ps("Get-Process uv -ErrorAction SilentlyContinue | Stop-Process -Force"),
        dry_run,
    )
}

pub fn cmd_logs_tail(env: Env, lines: u32, dry_run: bool) -> Result<()> {
    let insts = env.instances();
    if insts.len() > 1 {
        bail!("`logs tail` only supports one instance at a time — pass `ictrading` or `ftmo` ");
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

pub fn cmd_logs_pull(env: Env, dest: String, use_rsync: bool, dry_run: bool) -> Result<()> {
    let dest = shellexpand_home(&dest);
    if !dry_run {
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("failed to create destination dir {dest}"))?;
    }

    if use_rsync {
        let remote = format!(
            "{}:{}/logs/",
            crate::host(),
            to_forward_slashes(&crate::remote_project())
        );
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
                crate::host(),
                to_forward_slashes(&crate::remote_project()),
                inst
            );
            println!("==> scp {inst}.log -> {dest}");
            run_local("scp", &[remote, format!("{dest}/")], dry_run)?;
        }
        Ok(())
    }
}

pub fn cmd_shell(dry_run: bool) -> Result<()> {
    if dry_run {
        println!("[dry-run] ssh {}", crate::host());
        return Ok(());
    }
    let status = Command::new("ssh")
        .arg(crate::host())
        .status()
        .context("failed to spawn `ssh` ")?;
    if !status.success() {
        exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn task_name(instance: &str) -> String {
    format!("{}-{}", crate::task_prefix(), instance)
}

fn log_path(instance: &str) -> String {
    format!(r"{}\logs\{}.log", crate::remote_project(), instance)
}

fn run_remote(ps_command: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("[dry-run] ssh {} -- {}", crate::host(), ps_command);
        return Ok(());
    }

    let status = Command::new("ssh")
        .arg("-t")
        .arg(crate::host())
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

fn run_remote_script(script: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!(
            "[dry-run] ssh {} -- powershell -NoProfile -Command - <<'PS1'",
            crate::host()
        );
        println!("{script}");
        println!("PS1");
        return Ok(());
    }

    let mut child = Command::new("ssh")
        .arg(crate::host())
        .arg("powershell -NoProfile -Command -")
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to spawn `ssh` — is it installed and on your PATH?")?;

    child
        .stdin
        .take()
        .expect("stdin was requested as piped")
        .write_all(script.as_bytes())
        .context("failed to write script to remote powershell over ssh stdin")?;

    let status = child.wait().context("failed waiting on ssh")?;
    if !status.success() {
        bail!(
            "remote script exited with status {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
    }
    Ok(())
}

fn ps_single_quote(s: &str) -> String {
    s.replace('\'', "''")
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
