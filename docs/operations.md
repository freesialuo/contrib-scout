# Running contrib-scout

How to keep the shared index fresh, where contrib-scout keeps its data, and what it deliberately does not do.

## Scheduling a periodic sync

There is no daemon. A scheduled `sync --all-tracked` is the single background writer; agents then read with the default TTLs or `--offline`. Exit code `10` (new changes) is a success for a scheduler.

**Linux / macOS (cron)**, every 30 minutes. The job's output redirect fails, and the sync never runs, if the log directory is missing, so create it first:

```sh
mkdir -p "$HOME/.local/state"
```

Then add the job:

```cron
*/30 * * * * /usr/local/bin/contrib-scout --agent scheduler sync --all-tracked --max-requests 200 >> "$HOME/.local/state/contrib-scout-sync.log" 2>&1
```

Use the absolute path from `command -v contrib-scout`; cron has a minimal `PATH`, and `gh` must also be reachable and authenticated for that user.

**Windows (Task Scheduler)**, every 30 minutes, from PowerShell:

```powershell
$exe = (Get-Command contrib-scout).Source   # e.g. ...\npm\contrib-scout.cmd
$action = New-ScheduledTaskAction -Execute $exe -Argument '--agent scheduler sync --all-tracked --max-requests 200'
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 30)
Register-ScheduledTask -TaskName 'contrib-scout sync' -Action $action -Trigger $trigger
```

Or with `schtasks`:

```bat
schtasks /Create /SC MINUTE /MO 30 /TN "contrib-scout sync" /TR "contrib-scout --agent scheduler sync --all-tracked --max-requests 200"
```

The task runs as your user so it can use your `gh` login.

## Data location

All agents on a machine share one database file by default.

| | Database | Configuration |
|---|---|---|
| Linux | `~/.local/share/contrib-scout/contrib-scout.db` | `~/.config/contrib-scout/config.toml` |
| macOS | `~/Library/Application Support/contrib-scout/contrib-scout.db` | `~/Library/Application Support/contrib-scout/config.toml` |
| Windows | `%APPDATA%\contrib-scout\data\contrib-scout.db` | `%APPDATA%\contrib-scout\config\config.toml` |

Override with `--db` / `CONTRIB_SCOUT_DB` and `--config` / `CONTRIB_SCOUT_CONFIG`. The database (SQLite, WAL) holds the HTTP cache, issue index, policy and issue marks with full history, claims, gate logs, and the shared quota. `cache prune` never removes marks or history.

## What contrib-scout does not do

- It does not decide whether a project accepts AI contributions. No keyword matching, no classification: agents read the raw text and write marks.
- It does not write to GitHub: no comments, no PRs, no assignments, no remote claims. Claims and marks are local to the machine.
- It does not run a service, mirror GitHub, or scan large repositories in full.
- It does not use cached data for gates.
