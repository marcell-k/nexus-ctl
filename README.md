# nexus-ctl

A small Rust CLI that wraps the manual PowerShell commands.

## Build

```bash
cargo build --release
```

## Configuration

Defaults match the runbook (`Marci@windows.tail0212d0.ts.net`,
`C:\Users\Marci\Desktop\nexus-trade`, tasks named `nexus-trade-<instance>`).

| Variable                     | Default                                    |
|-------------------------------|--------------------------------------------|
| `NEXUS_CTL_HOST`              | `Marci@windows.tail0212d0.ts.net`           |
| `NEXUS_CTL_REMOTE_PROJECT`    | `C:\Users\Marci\Desktop\nexus-trade`        |
| `NEXUS_CTL_TASK_PREFIX`       | `nexus-trade`                               |

## Usage

```
nexus-ctl [--dry-run] <COMMAND>
```

`--dry-run` prints the exact command that would run without executing it —
useful for sanity-checking before you touch a live trading process.

### Start / stop / restart

```bash
nexus-ctl start              # both instances (default)
nexus-ctl start ictrading    # just one
nexus-ctl stop ftmo          # stops, then checks for a lingering uv.exe
nexus-ctl stop --force       # stop both, and force-kill uv.exe if it lingers
nexus-ctl restart ftmo       # stop (with force-kill) then start
```

### Status

```bash
nexus-ctl status             # Get-ScheduledTaskInfo for both + any uv.exe processes
```

### Kill a stuck process directly

```bash
nexus-ctl kill                # Get-Process uv | Stop-Process -Force
```

### Logs

```bash
nexus-ctl logs tail ictrading            # live tail, last 50 lines then follow
nexus-ctl logs tail ftmo -n 200          # follow with a bigger backlog

nexus-ctl logs pull                      # scp both logs to ~/Desktop/nexus-trade-logs
nexus-ctl logs pull ictrading --dest ~/Desktop
nexus-ctl logs pull --rsync              # incremental sync of the whole logs/ folder
```

Note: `logs tail` only accepts a single instance at a time (a live
`Get-Content -Wait` naturally corresponds to one file/session).

### Drop into a plain SSH session

```bash
nexus-ctl shell               # equivalent to `ssh Marci@windows.tail0212d0.ts.net`
```
