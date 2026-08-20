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
Write-Host "[$Instance] PID $targetPid -- sending graceful stop signal (same as Ctrl+C)"

$senderBody = @'
param($TargetPid)
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
$attached = [NexusCtl.NexusCtrlC]::AttachConsole([uint32]$TargetPid)
if (-not $attached) {
    exit 1
}
[NexusCtl.NexusCtrlC]::SetConsoleCtrlHandler([IntPtr]::Zero, $true) | Out-Null
[NexusCtl.NexusCtrlC]::GenerateConsoleCtrlEvent(0, 0) | Out-Null
Start-Sleep -Milliseconds 500
[NexusCtl.NexusCtrlC]::FreeConsole() | Out-Null
exit 0
'@
$encodedSender = [Convert]::ToBase64String([System.Text.Encoding]::Unicode.GetBytes($senderBody))

# --- Attempt 1: direct, isolated child process ---------------------------
$directProc = Start-Process -FilePath 'powershell.exe' `
    -ArgumentList @('-NoProfile', '-EncodedCommand', $encodedSender, $targetPid) `
    -WindowStyle Hidden -Wait -PassThru

if ($directProc.ExitCode -eq 0) {
    Write-Host "[$Instance] signal delivered directly."
} else {
    # --- Attempt 2: relay through a clone of the real task ---------------
    # Same session/security context as $TaskName, so AttachConsole works.
    Write-Host "[$Instance] direct delivery failed (exit $($directProc.ExitCode)) -- likely a different session (e.g. Session 0 for an unattended task). Relaying via a temporary scheduled task instead."

    $senderTaskName = "NexusCtrlCSender-$([guid]::NewGuid().ToString('N').Substring(0,8))"
    try {
        [xml]$taskXml = Export-ScheduledTask -TaskName $TaskName
        $ns = New-Object System.Xml.XmlNamespaceManager($taskXml.NameTable)
        $ns.AddNamespace('t', 'http://schemas.microsoft.com/windows/2004/02/mit/task')
        $nsUri = $ns.LookupNamespace('t')

        $actionsNode = $taskXml.SelectSingleNode('//t:Actions', $ns)
        $actionsNode.RemoveAll()
        $execNode = $taskXml.CreateElement('Exec', $nsUri)
        $cmdNode = $taskXml.CreateElement('Command', $nsUri)
        $cmdNode.InnerText = 'powershell.exe'
        $argNode = $taskXml.CreateElement('Arguments', $nsUri)
        $argNode.InnerText = "-NoProfile -WindowStyle Hidden -EncodedCommand $encodedSender $targetPid"
        $execNode.AppendChild($cmdNode) | Out-Null
        $execNode.AppendChild($argNode) | Out-Null
        $actionsNode.AppendChild($execNode) | Out-Null

        $triggersNode = $taskXml.SelectSingleNode('//t:Triggers', $ns)
        $triggersNode.RemoveAll()
        $timeTrigger = $taskXml.CreateElement('TimeTrigger', $nsUri)
        $startNode = $taskXml.CreateElement('StartBoundary', $nsUri)
        $startNode.InnerText = (Get-Date).AddSeconds(2).ToString('yyyy-MM-ddTHH:mm:ss')
        $timeTrigger.AppendChild($startNode) | Out-Null
        $triggersNode.AppendChild($timeTrigger) | Out-Null

        Register-ScheduledTask -TaskName $senderTaskName -Xml $taskXml.OuterXml -Force | Out-Null
        Start-ScheduledTask -TaskName $senderTaskName

        $relayDeadline = (Get-Date).AddSeconds(15)
        do {
            Start-Sleep -Milliseconds 500
            $info = Get-ScheduledTask -TaskName $senderTaskName -ErrorAction SilentlyContinue
        } while ($info -and $info.State -eq 'Running' -and (Get-Date) -lt $relayDeadline)

        $result = (Get-ScheduledTaskInfo -TaskName $senderTaskName -ErrorAction SilentlyContinue).LastTaskResult
        Write-Host "[$Instance] relay task finished (result: $result)."
    } catch {
        Write-Host "[$Instance] WARNING: relay via scheduled task failed: $_"
        Write-Host "[$Instance] WARNING: this usually means the task uses a stored-password logon and Register-ScheduledTask needs -User/-Password to recreate it -- check the task's 'Run whether user is logged on or not' credentials in Task Scheduler."
    } finally {
        Unregister-ScheduledTask -TaskName $senderTaskName -Confirm:$false -ErrorAction SilentlyContinue
    }
}

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
