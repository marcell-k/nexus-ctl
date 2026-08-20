use std::io::Write;
use std::process::{exit, Command, Stdio};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use clap::{Subcommand, ValueEnum};

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(s)
        .context("invalid base64 from remote script")
}

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

Add-Type -Name Win32CtrlC -Namespace NexusCtl -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool AttachConsole(uint dwProcessId);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool SetConsoleCtrlHandler(IntPtr HandlerRoutine, bool Add);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool GenerateConsoleCtrlEvent(uint dwCtrlEvent, uint dwProcessGroupId);
'@

[void][NexusCtl.Win32CtrlC]::FreeConsole()
if (-not [NexusCtl.Win32CtrlC]::AttachConsole([uint32]$targetPid)) {
    $err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
    Write-Host "[$Instance] WARNING: AttachConsole failed (err=$err) -- target has no console, can't send CTRL_C. Force-killing."
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    Stop-Process -Id $targetPid -Force -ErrorAction SilentlyContinue
    exit 1
}

Write-Host "[$Instance] PID $targetPid -- sending CTRL_C"
[void][NexusCtl.Win32CtrlC]::SetConsoleCtrlHandler([IntPtr]::Zero, $true)   # ignore CTRL_C in this shell too
[void][NexusCtl.Win32CtrlC]::GenerateConsoleCtrlEvent(0, 0)                # 0 = CTRL_C_EVENT, group 0 = whole console
Start-Sleep -Milliseconds 500
[void][NexusCtl.Win32CtrlC]::FreeConsole()
[void][NexusCtl.Win32CtrlC]::SetConsoleCtrlHandler([IntPtr]::Zero, $false)

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

Write-Host "[$Instance] WARNING: still running after $GraceSeconds second(s) -- force-killing the whole process tree now."
Write-Host "[$Instance] WARNING: CTRL_C was sent but app didn't exit in time -- verify open positions manually."
Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
$cmdProc = Get-CimInstance Win32_Process -Filter "Name='cmd.exe'" |
    Where-Object { $_.CommandLine -like "*run-$Instance*" } |
    Select-Object -First 1

if ($cmdProc) {
    Write-Host "[$Instance] killing process tree from cmd.exe PID $($cmdProc.ProcessId)"
    taskkill /PID $cmdProc.ProcessId /T /F
} else {
    Write-Host "[$Instance] could not locate parent cmd.exe -- falling back to killing uv.exe PID only"
    Stop-Process -Id $targetPid -Force -ErrorAction SilentlyContinue
}
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

        #[arg(long, default_value_t = 70)]
        grace: u32,

        #[arg(long)]
        force: bool,
    },

    Status,
    Logs {
        #[command(subcommand)]
        action: LogsAction,
    },
    Shell,
    Processes,
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
            &format!("Start-ScheduledTask -TaskName '{}'", task_name(inst)),
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
                &format!(
                    "Stop-ScheduledTask -TaskName '{}'; \
         Get-CimInstance Win32_Process -Filter \"Name='uv.exe'\" | \
         Where-Object {{ $_.CommandLine -like '*.env.{}*' }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}",
                    task_name(inst),
                    inst
                ),
                dry_run,
            )?;
            continue;
        }

        println!("==> stopping {inst} gracefully (grace period: {grace}s)");
        let header = format!(
            "$Instance = '{}'\n$EnvFilter = '{}'\n$GraceSeconds = {}\n$TaskName = '{}'\n$RemoteProject = '{}'\n",
            ps_single_quote(inst),
            ps_single_quote(&format!(".env.{inst}")),
            grace,
            ps_single_quote(&task_name(inst)),
            ps_single_quote(&crate::remote_project()),
        );
        run_remote_script(&format!("{header}{GRACEFUL_STOP_SCRIPT}"), dry_run)?;
    }
    Ok(())
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
    run_remote(&cmd_parts.join("; "), dry_run)
}

pub fn cmd_processes(dry_run: bool) -> Result<()> {
    run_remote(
        "Get-CimInstance Win32_Process -Filter \"Name='uv.exe' or Name='cmd.exe'\" | \
         Where-Object { $_.CommandLine -like '*nexus-trade*' } | \
         Select-Object ProcessId, Name, CommandLine | Format-Table -AutoSize",
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
        &format!("Get-Content '{}' -Wait -Tail {}", log_path(inst), lines),
        dry_run,
    )
}

pub fn cmd_logs_pull(env: Env, _dest: String, _use_rsync: bool, dry_run: bool) -> Result<()> {
    let dest_base = "/Users/marci/Trading/logs";

    if dry_run {
        println!(
            "[dry-run] ssh {} -- powershell script to copy shared logs -> extract to {dest_base}",
            crate::host()
        );
        return Ok(());
    }

    let instances = env.instances();
    println!(
        "==> pulling logs for {:?} (handling locked files) -> {dest_base}",
        instances
    );

    let filter_items = instances
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let ps_filter = format!("@({filter_items})");

    let ps_script = format!(
        r#"
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$targetInstances = {ps_filter}
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
$tempZip = [System.IO.Path]::GetTempFileName() + '.zip'

New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

Get-ChildItem -Path '{}\logs' -Recurse -File | ForEach-Object {{
    $file = $_
    $matchesFilter = $false
    foreach ($inst in $targetInstances) {{
        if ($file.Name -like "*$inst*" -or $file.DirectoryName -like "*$inst*") {{
            $matchesFilter = $true
            break
        }}
    }}

    if ($matchesFilter) {{
        $relPath = $file.FullName.Substring('{}\logs'.Length).TrimStart('\')
        $targetFile = Join-Path $tempDir $relPath
        $targetFolder = [System.IO.Path]::GetDirectoryName($targetFile)

        if (-not (Test-Path $targetFolder)) {{
            New-Item -ItemType Directory -Path $targetFolder -Force | Out-Null
        }}

        $srcStream = [System.IO.File]::Open($file.FullName, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        $destStream = [System.IO.File]::Create($targetFile)
        $srcStream.CopyTo($destStream)
        $srcStream.Close()
        $destStream.Close()
    }}
}}

if ((Get-ChildItem -Path $tempDir -Recurse -File).Count -eq 0) {{
    Write-Error "No matching log files found on remote host."
    exit 1
}}

Compress-Archive -Path "$tempDir\*" -DestinationPath $tempZip -Force
$bytes = [System.IO.File]::ReadAllBytes($tempZip)
[Convert]::ToBase64String($bytes)

Remove-Item $tempDir -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $tempZip -Force -ErrorAction SilentlyContinue
"#,
        crate::remote_project(),
        crate::remote_project()
    );

    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg(crate::host())
        .arg("powershell -NoProfile -Command -")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn ssh process")?;

    child
        .stdin
        .take()
        .expect("stdin handle lost")
        .write_all(ps_script.as_bytes())
        .context("failed to write script to stdin")?;

    let output = child.wait_with_output().context("failed waiting on ssh")?;

    if !output.status.success() {
        bail!(
            "failed to archive logs on remote host: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // stdout is a base64 string (PowerShell may add trailing newline/whitespace).
    let b64 = String::from_utf8_lossy(&output.stdout);
    let b64 = b64.trim();
    if b64.is_empty() {
        bail!(
            "remote script produced no output — stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let zip_bytes = base64_decode(b64).context("failed to decode base64 zip data from remote")?;

    let temp_zip_path = std::env::temp_dir().join("nexus_logs_temp.zip");
    std::fs::write(&temp_zip_path, &zip_bytes)
        .context("failed to save temporary zip file locally")?;

    std::fs::create_dir_all(dest_base)?;
    let status = Command::new("unzip")
        .arg("-o")
        .arg(&temp_zip_path)
        .arg("-d")
        .arg(dest_base)
        .status()
        .context("failed to run local `unzip` utility")?;

    let _ = std::fs::remove_file(temp_zip_path);

    if !status.success() {
        bail!("failed to extract log files into {dest_base}");
    }

    println!("==> successfully updated logs in {dest_base}");
    Ok(())
}

pub fn cmd_shell(dry_run: bool) -> Result<()> {
    if dry_run {
        println!("[dry-run] ssh {}", crate::host());
        return Ok(());
    }
    let status = Command::new("ssh")
        .arg("-o")
        .arg("LogLevel=ERROR")
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

fn run_remote(ps_script: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!(
            "[dry-run] ssh {} -- powershell -EncodedCommand <base64> for:\n{}",
            crate::host(),
            ps_script
        );
        return Ok(());
    }

    let status = Command::new("ssh")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg(crate::host())
        .arg(ps_encoded(ps_script))
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

fn ps_encoded(script: &str) -> String {
    let full = format!("$ProgressPreference = 'SilentlyContinue'\n{script}");
    let utf16_bytes: Vec<u8> = full.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    format!(
        "powershell -NoProfile -EncodedCommand {}",
        STANDARD.encode(utf16_bytes)
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
        .arg("-o")
        .arg("LogLevel=ERROR")
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
