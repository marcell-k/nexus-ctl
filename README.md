# nexus-ctl

A small Rust CLI that wraps the manual SSH / PowerShell / scp / rsync
commands from the `nexus-trade` remote-control runbook, so you type e.g.
`nexus-ctl stop ftmo --force` instead of re-typing PowerShell one-liners.

It does **not** implement SSH itself — every subcommand just builds the same
command line you'd type by hand and runs your local `ssh`, `scp`, or `rsync`
binary with inherited stdio. Your existing SSH keys, agent, and
`~/.ssh/config` all keep working unchanged, and Ctrl+C / live tail / prompts
behave exactly as they do today.

## Build

```bash
cargo build --release
# binary at ./target/release/nexus-ctl
```

Requires a Rust toolchain (edition 2021). Tested with rustc 1.75+.

Optionally install it onto your PATH:

```bash
cargo install --path .
```

## Configuration

Defaults match the runbook (`Marci@windows.tail0212d0.ts.net`,
`C:\Users\Marci\Desktop\nexus-trade`, tasks named `nexus-trade-<instance>`).

| Variable                     | Default                                    |
|-------------------------------|--------------------------------------------|
| `NEXUS_CTL_HOST`              | `Marci@windows.tail0212d0.ts.net`           |
| `NEXUS_CTL_REMOTE_PROJECT`    | `C:\Users\Marci\Desktop\nexus-trade`        |
| `NEXUS_CTL_TASK_PREFIX`       | `nexus-trade`                               |

There are two ways to override these, and they can be mixed:

1. **Shell environment variables** — `export NEXUS_CTL_HOST=...`. Always wins.
2. **A `.env` file at `~/.config/nexus-ctl/.env`** — copy `.env.example` there
   and edit it. Picked up automatically on every run, from any directory, but
   only fills in values that aren't already set in your shell environment.

Precedence: **shell env var → `~/.config/nexus-ctl/.env` → built-in default.**

```bash
mkdir -p ~/.config/nexus-ctl
cp .env.example ~/.config/nexus-ctl/.env
# then edit ~/.config/nexus-ctl/.env
```

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

nexus-ctl stop                       # graceful stop, both instances, 60s grace period
nexus-ctl stop ftmo                  # just one
nexus-ctl stop ftmo --grace 120      # give it longer to close positions
nexus-ctl stop ftmo --force          # skip the grace period entirely — emergency only

nexus-ctl restart ftmo               # graceful stop, then start
nexus-ctl restart ftmo --grace 120
```

**How `stop` works:** `Stop-ScheduledTask` and `Stop-Process -Force` both hard-kill
the process — the app never gets a chance to run its own shutdown code, which
means open positions may not get closed. To avoid that, `stop` instead:

1. Finds the exact `uv.exe` PID for that instance (filtered by its `--env`
   argument, so `ictrading` and `ftmo` are never confused with each other).
2. Sends it a real `CTRL_C_EVENT` — the same signal your keyboard sends when
   you press Ctrl+C in an interactive session — so the app's own shutdown /
   close-positions logic runs.
3. Waits up to `--grace` seconds (default 60) for it to exit on its own.
4. Only if it's still running after that, force-kills it — and prints a
   clear **WARNING** so you know it didn't get to shut down cleanly and
   should check open positions manually.

`--force` skips straight to the hard-kill with no grace period — use it only
if you've already confirmed positions are flat, or in a genuine emergency.

This assumes the app already handles Ctrl+C (`KeyboardInterrupt` in Python)
for its normal graceful-shutdown path when run interactively — `stop` just
delivers that same signal remotely. **Test it once on a non-critical
instance before relying on it live.**

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
nexus-ctl logs last ftmo                 # last 30 lines, no follow
nexus-ctl logs last -n 100               # last 100 lines of BOTH logs

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

## What each command actually runs

Every subcommand mirrors a snippet from the runbook 1:1, e.g.:

- `start` → `Start-ScheduledTask -TaskName "nexus-trade-<instance>"`
- `stop` → `Stop-ScheduledTask ...` then `Get-Process uv ...` (and
  `Stop-Process -Force` if `--force` is given or the process lingers)
- `logs tail` → `Get-Content <path> -Wait -Tail <n>`
- `logs pull` → `scp` per file, or `rsync -avz` for the whole `logs/` folder

Use `--dry-run` any time you want to see the exact line before it runs.
