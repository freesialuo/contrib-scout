# contrib-scout

A command-line tool and Rust library that helps many AI agents find open-source issues worth contributing to, without each agent hammering the GitHub API on its own.

> [!NOTE]
> `contrib-scout` is not affiliated with or endorsed by GitHub. It is **read-only towards GitHub**: it never comments, opens pull requests, assigns, or claims anything on GitHub. Claims, marks, and history live only in a local database.

> [!IMPORTANT]
> `contrib-scout` **never judges a project's AI policy itself.** It fetches the raw policy text, stores the marks that agents write after reading that text, and repeats those marks as reminders. Whether a project accepts AI-assisted contributions is always decided by the agent (or human) who read the source.

## Why

When several agents look for contributions in parallel, each one tends to re-search GitHub, re-read the same `CONTRIBUTING.md`, and re-discover that a project bans AI-generated pull requests. `contrib-scout` gives them one shared memory:

- **One local, shared SQLite database.** Every agent on the machine reads the same cache (ETags, TTLs, negative cache), the same shared quota table, and the same claim table. A key is fetched once, even when two processes ask at the same moment.
- **Discovery without hammering GitHub.** `discover` and `candidates` run cheapest-first and mostly from cache; `candidates` makes zero requests. A scheduled `sync` is the only background writer.
- **Agent-written AI-policy marks.** An agent reads the raw policy files (`policy files`) and records its reading (`policy set`) with a stance, evidence, scope, and requirements. From then on, every command that touches the repository (or any repository of a marked organization) carries a reminder in `meta.reminders`. Unmarked repositories carry an `unmarked` reminder telling the agent how to mark them.
- **Agent-written issue marks.** An agent that read an issue and decided it is not worth doing can mark it `skip` with a reason; later agents skip it instead of reading it again. Good issues can be `shortlist`ed.
- **Live gates.** `gate start` (before starting work) and `gate pr` (before opening a PR) always hit GitHub live and fail closed. They return raw GitHub data and the current policy text for the agent to read, and block on an unmarked policy, a ban, a skipped issue, another agent's claim, or an existing PR.

## Installation

`contrib-scout` talks to GitHub through the [GitHub CLI](https://cli.github.com). **It requires an authenticated `gh`** (`gh auth login`, check with `gh auth status`).

### From npm (recommended)

Install globally with Node.js 22 or newer:

```sh
npm install -g @freesialuo/contrib-scout
contrib-scout --help
```

Or run without installing globally:

```sh
npx @freesialuo/contrib-scout --help
```

The `@freesialuo/contrib-scout` package bundles prebuilt native binaries for:

- `darwin-arm64` (macOS Apple Silicon)
- `darwin-x64` (macOS Intel)
- `linux-arm64` (Linux glibc, Ubuntu 22.04 baseline)
- `linux-x64` (Linux glibc, Ubuntu 22.04 baseline)
- `win32-x64` (Windows x64)

### From source (Rust)

Building from source requires Rust 1.95 or newer (`rust-version` in `Cargo.toml`). Check with `rustc --version`. With rustup, update using `rustup update stable`. Older toolchains stop with `rustc 1.xx is not supported by the following packages: ... requires rustc 1.95`.

```sh
git clone https://github.com/freesialuo/contrib-scout.git
cd contrib-scout
cargo install --path crates/contrib-scout
contrib-scout --help
```

Or run from a checkout without installing:

```sh
cargo run --release -p contrib-scout -- --help
```

## Quick start

```sh
# Identify yourself once per agent (or pass --agent on each call).
export CONTRIB_SCOUT_AGENT=agent-7

# 1. Discover repositories and issues (cached, cheapest checks first).
contrib-scout --json discover --lang rust --label "good first issue" --limit 20

# 2. A repository looks interesting but carries an `unmarked` reminder:
#    read its raw policy text yourself ...
contrib-scout --json policy files owner/repo

#    ... then record your reading, with evidence.
contrib-scout policy set owner/repo --stance disclose \
  --evidence CONTRIBUTING.md:40-42 \
  --require disclose_in_pr --require assisted_by_trailer \
  --note "AI allowed if disclosed in the PR and commits carry Assisted-by"

# 3. Rank indexed issues from local data only (zero requests).
contrib-scout --json candidates --lang rust --limit 20

# 4. Read an issue; skip it for everyone if it is not worth doing.
contrib-scout --json issue show 'owner/repo#123'
contrib-scout issue mark 'owner/repo#123' --verdict skip --reason too_large \
  --note "Needs a redesign of the storage layer"

# 5. Before starting work: live gate, acquires a local claim.
contrib-scout --json gate start 'owner/repo#124'

# 6. Before opening a PR: live gate, returns the current rules and PR template.
contrib-scout --json gate pr 'owner/repo#124' --branch fix-124
```

Issue references (`<ref>`) accept `OWNER/REPO#N` or a GitHub issue URL (`https://github.com/OWNER/REPO/issues/N`). Quote `#` references in shells that treat `#` as a comment.

## CLI reference

Run `contrib-scout <command> --help` for full option details.

### Discovery and repositories

| Command | Description |
|---|---|
| `discover [--lang L] [--topic T] [--label L]... [--limit N] [--explain] [--include-marked]` | Search cheapest-first: search (cached 24h) → local policy marks (zero requests; `bans` repositories excluded with a count) → repository metadata (archived and forks dropped) → activity → issue index → local score |
| `candidates [--min-score N] [--lang L] [--limit N] [--include-marked]` | Rank indexed issues from local data only; zero requests |
| `repo show <owner/repo>` | Metadata, contributability metrics, and the policy mark (`--fresh` ignores the TTL) |

`discover` and `candidates` exclude repositories marked `bans` and issues marked `skip` by default and report how many were excluded; `--include-marked` shows them. Shortlisted issues sort first.

### AI policy marks (§5)

| Command | Description |
|---|---|
| `policy files <owner/repo>` | Fetch the raw candidate policy files (org `{owner}/.github` first, then the repository: `AI_POLICY.md`, `LLM_POLICY.md`, `CONTRIBUTING.md` in root, `.github/` and `docs/`, `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, README, PR templates). Returns path, permalink, and text (possibly truncated, flagged). No interpretation |
| `policy show <owner/repo\|org>` | Current mark and its history |
| `policy set <owner/repo\|org> --stance S --evidence PATH[:LINES]... --note T [--forbid-label L]... [--require R]... [--scope K=V]...` | Write or overwrite a mark. At least one `--evidence`; every write appends history |
| `policy unset <owner/repo\|org>` | Remove a mark (history is kept) |
| `policy due` | List marks whose review is due |

- `--stance`: `bans`, `restricts`, `disclose`, `allows`, `welcomes`, `none_found` (for `none_found`, record the paths you checked as evidence).
- `--evidence`: a repository path or an external URL, optionally with lines (`CONTRIBUTING.md:10-12`). A line range covers at most 3 lines (the excerpt limit); quote the decisive lines, and give several `--evidence` flags for separate passages.
- `--require`: `disclose_in_pr`, `assisted_by_trailer`, `human_written_description`, `human_written_replies`, `maintainer_approval_on_issue`, `label_pr=LABEL`.
- `--scope K=V`: `code`, `pr_text`, `comments`, `autonomous_agents` take `unspecified`, `allowed`, `disclose`, `human_written`, or `forbidden`; `needs_prior_approval=true|false`; `forbidden_label=L` (same as `--forbid-label L`, for policies such as "no AI on good first issues"; matched ignoring case, `-`/`_` versus spaces, and extra whitespace, so `good-first-issue` matches `Good First Issue`).
- An organization mark applies to all of its repositories; a repository mark overrides it.
- Default review periods: `bans` 90 days; `restricts`/`disclose` 30 days; `allows`/`welcomes`/`none_found` 14 days. Expired marks are never deleted or rewritten; their reminders gain `review_due: true`.

### Issues and issue marks (§5b)

| Command | Description |
|---|---|
| `issue show <ref>` | Issue body, recent comments, linked PRs, plus the issue mark and repository mark reminders (`--fresh` ignores the TTL) |
| `issue mark <ref> --verdict skip\|shortlist [--reason R] --note T [--until D]` | Write or overwrite an issue mark; every write appends history |
| `issue unmark <ref>` | Remove an issue mark |
| `issue marks [--verdict V] [--repo R]` | List issue marks |

Skip reasons: `too_large`, `unclear`, `needs_special_env`, `out_of_scope`, `duplicate`, `maintainer_unresponsive`, `already_addressed`, `other`. Issue marks do not expire unless `--until` is given. When the index sees that a marked issue's `updated_at` or comment count changed since it was marked, the reminder gains `changed_since_mark: true` (no extra request). Closed, assigned, and already-PR'd issues are detected by the index and need no mark.

### Gates and claims (§8, §9)

| Command | Description |
|---|---|
| `gate start <ref> --agent ID` | Live checks before starting work; on `pass`, acquires or renews your local claim |
| `gate pr <ref> --agent ID [--branch B] [--approval-url URL]` | Live checks before opening a PR; returns the full current policy text, PR template, PR requirements, and a checklist. `--branch` excludes your own PR from the linked-PR check and is recorded on your claim; `--approval-url` points to the maintainer's approval when the mark requires one |
| `claim list [--all]` | List active claims (only yours when `--agent` is set, unless `--all`) |
| `claim renew <ref> --agent ID` | Extend your lease (`claim_lease`, default 48h) |
| `claim release <ref> --agent ID` | Release your claim |

Gates require an agent id (`--agent` or `CONTRIB_SCOUT_AGENT`; missing is a usage error, exit 2). A ref that turns out to be a pull request is a usage error too (exit 2, not logged); `issue show` still shows it, with `is_pull_request: true`. Both gates check live: the issue is open, not marked `skip`, not assigned to someone other than the authenticated GitHub user, has no open or merged linked PR (closed-unmerged PRs are only reported), the repository is not archived, a policy mark exists and is not `bans`, and the issue's labels are not in the mark's forbidden labels. `gate start` blocks when another agent holds the claim; `gate pr` requires that you hold it.

Report-only `observations` (`{kind, severity, detail, url}`, severity `info` or `warning`, never blocking): comments that look like someone taking the issue (`comment_claim`, a phrase heuristic), the issue assigned to you (`assigned_to_you`), maintainer comments to read for approval (`approval_evidence`), your own excluded PR (`own_pr_excluded`), closed linked PRs, non-blocking issue marks, a mark past its review date, and truncated policy files.

When the mark requires maintainer approval (`--scope needs_prior_approval=true` or `--require maintainer_approval_on_issue`), `gate start` reports it and `gate pr` blocks with `approval_missing` until you pass `--approval-url URL` (an `https` link to the approval; the CLI records it but does not verify it). Your own PR is excluded only with `--branch`: a linked PR is yours when its author is the authenticated GitHub user and its head branch (read live, at most 5 lookups) equals `--branch`. The login comes from one live `GET /user` per gate and is never cached; gates therefore need an authenticated `gh`/token.

The verdict is `pass` (exit 0), `blocked` (exit 20; reasons `policy_unmarked`, `policy_bans`, `issue_skipped`, `issue_closed`, `assigned_to_other`, `pr_exists`, `repo_archived`, `forbidden_label`, `claimed_by_other`, `claim_not_held`, `approval_missing`, `human_required`), or `error` (exit 1: network or quota trouble, fail closed; the envelope has `data: null` and `error.code`, for example `network_unavailable`). `gate pr` returns `blocked: human_required` when the mark says a human must write part of the contribution: scope `pr_text` or `comments` set to `human_written` or `forbidden`, `autonomous_agents: forbidden`, `code: forbidden`, or the requirements `human_written_description` / `human_written_replies`. `code: forbidden` (with a stance other than `bans`, which already blocks) also blocks `gate start` with `human_required`, since an AI agent cannot write the code at all. Every gate run, errors included, is appended to `gate_log`. Gates refuse `--offline` and never fall back to the cache; a `304 Not Modified` counts as a live confirmation.

The gate report (`data`) holds `verdict`, `blocked_by` (`{reason, detail}`), `observations`, `requirements` (from the mark: a preview on `start`), `checklist` (`gate pr` only: requirements, restricted scope, approval, and every policy file to read or PR template to follow), `claim`, `viewer`, and `raw`: the issue body, the last `gate_comments` comments verbatim, linked PRs (timeline events plus closing references), the repository, the policy mark (note, evidence excerpts, pinned URLs), the issue mark, and the policy files (`gate start`: list only; `gate pr`: full current text). A gate costs a bounded number of requests: `GET /user`, the repository, the issue with up to two comment pages and its timeline pages, one GraphQL query for the issue's closing references (`closedByPullRequestsReferences`: PRs linked in the Development sidebar may have no usable timeline event), one `commits/HEAD` check per policy source (GraphQL only when a HEAD moved), and on `gate pr --branch` up to 5 PR lookups.

### Index, tracking, and maintenance

| Command | Description |
|---|---|
| `track add\|rm <owner/repo>` / `track list` | Repositories refreshed by the scheduled sync |
| `sync [<owner/repo>...] [--all-tracked] [--max-requests N]` | Incrementally refresh the issue index (`since` watermark with an overlap window) for the given or all tracked repositories |
| `quota` | Shared quota and window usage across all agents (requests are refused when the last observed `remaining`, minus requests other agents have in flight since that observation, reaches the reserve: default core 500, search 5, graphql 500. The in-flight count is an estimate at one-second resolution, so concurrent agents can overshoot the reserve by roughly the number of requests in flight at once; the all-agent per-minute and per-hour windows bound that) |
| `cache stats` / `cache prune [--older-than D]` | Inspect or prune cached data (default `30d`; never marks or history) |
| `config get <key>` / `config set <key> <value>` | Read or change configuration (for example `claim_lease`, `max_requests`, `mark_ttl_bans`) |

### Global options

| Option | Env / notes |
|---|---|
| `--json` | Write one `contrib-scout/v1` JSON envelope (conflicts with `--ndjson`) |
| `--ndjson` | Write one envelope per output record |
| `--db <path>` | Shared SQLite database path (`CONTRIB_SCOUT_DB`) |
| `--config <path>` | TOML configuration path (`CONTRIB_SCOUT_CONFIG`) |
| `--offline` | Read only the local cache; never make requests. Gates refuse this mode |
| `--fresh` | Refresh requested data regardless of TTL (conflicts with `--offline`) |
| `--max-requests <n>` | Cap requests for this invocation, including retries (default from config, `50`) |
| `-v`, `--verbose` | Request metrics on stderr |
| `--agent <id>` | Agent id for claims, mark history, and request accounting (`CONTRIB_SCOUT_AGENT`) |
| `--no-color` | Disable ANSI color in human output |

## JSON output

`--json` writes a single envelope:

```json
{
  "schema": "contrib-scout/v1",
  "meta": {
    "as_of": "2026-11-25T12:00:00Z",
    "requests": 1,
    "served_304": 1,
    "partial": false,
    "warnings": [],
    "quota": [],
    "reminders": [
      {
        "kind": "marked",
        "repo": "owner/repo",
        "target": { "kind": "org", "name": "owner" },
        "stance": "bans",
        "note": "No AI-generated contributions of any kind",
        "marked_at": "2026-11-20T09:00:00Z",
        "marked_by": "agent-3",
        "evidence_urls": ["https://github.com/owner/.github/blob/<sha>/AI_POLICY.md#L1-L4"],
        "review_due": false
      },
      {
        "kind": "unmarked",
        "repo": "owner/other",
        "evidence_urls": [],
        "review_due": false,
        "hint": "run contrib-scout policy files owner/other, read the text, then mark it with contrib-scout policy set"
      }
    ]
  },
  "data": {}
}
```

- `meta.reminders` is filled by every command that touches a repository: `marked` (with stance, note, evidence links, `review_due`) or `unmarked` (with a `hint`). Human output prints the same as a warning line, for example `⚠ owner/repo: Bans AI — "No AI-generated contributions" (marked 2026-11-20)`.
- `meta.partial` and `meta.warnings` say when some data could not be refreshed and came from the cache (stale) or is missing. Always check them.
- `meta.requests` / `meta.served_304` count this invocation's requests; `meta.quota` is the last observed shared quota (filled by every command that can reach the network, as it stands after the command's requests, and by `quota`; local-only commands leave it empty).
- On failure, `data` is `null` and `error` carries `{code, message, exit_code}`.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success; for gates, `pass` |
| `10` | New changes or new candidates found |
| `20` | Gate `blocked` |
| `1` | Error (for gates: fail closed on network or quota trouble) |
| `2` | Usage or validation error |

## Scheduling a periodic sync

There is no daemon. A scheduled `sync --all-tracked` is the single background writer; agents then read with the default TTLs or `--offline`. Exit code `10` (new changes) is a success for a scheduler.

**Linux / macOS (cron)**, every 30 minutes:

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

## Companion agent skill

[`skills/contrib-scout/SKILL.md`](skills/contrib-scout/SKILL.md) tells Claude agents how to drive `contrib-scout`: always `--json`, mark policies after reading them, skip issues for later agents, and obey the gates.

## Development

The Cargo workspace separates the library [`crates/contrib-scout-core`](crates/contrib-scout-core) from the CLI [`crates/contrib-scout`](crates/contrib-scout). All tests run offline against a fixture transport; one live smoke test is `#[ignore]`d.

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
node --test scripts/npm-release.test.mjs
```

Changes to the `contrib-scout/v1` JSON structure should stay backwards compatible for scripts and the skill.

## Releasing

Tag pushes matching `vX.Y.Z` or `vX.Y.Z-<prerelease>` (no build metadata) trigger the **Release** workflow, [`.github/workflows/release.yml`](.github/workflows/release.yml). It ships the GitHub Release and the npm package together, from the same tested bytes:

1. **version** checks that the tag, `npm/package.json`, and every Cargo workspace package agree, and runs `node --test scripts/*.test.mjs`.
2. **native** builds and tests release binaries for all five targets.
3. **pack** stages and packs `@freesialuo/contrib-scout`, verifies its contents and SHA-512 integrity, smoke-installs it offline, then builds the GitHub Release assets from the same binaries: `contrib-scout-vX.Y.Z-<target>.tar.gz` (`.zip` for `win32-x64`, each holding the binary, README, and licenses), the exact npm tarball, and `SHA256SUMS`.
4. **draft** creates the GitHub Release as a **draft** with those assets and generated notes (marked prerelease when the tag has a `-` suffix).
5. **publish** publishes the tarball through npm trusted publishing (OIDC, with provenance). Stable versions go to dist-tag `latest`, prereleases to `next`. The `npm` environment requires a maintainer's approval under **Review deployments**.
6. **release** re-checks the draft's assets and makes it public only after npm holds the same tarball.

Every step can be re-run. An existing npm version passes only if its tarball is byte-identical. An existing draft is repaired to match the run's assets; it is private, and it is only made public after npm matches. A published GitHub Release is never changed: re-running against it passes only if its assets are identical, and fails otherwise. After a partial failure, use **Re-run failed jobs** so the run reuses its own artifacts.

If the repository variable `NPM_TRUSTED_PUBLISHING_ENABLED` is not `true`, the run stops after **draft**. Nothing becomes public, so GitHub and npm cannot diverge. The run summary notes this, and the verified tarball stays available in the `npm-package` artifact. Once you set the variable, re-run the workflow (or finish the release by hand).

One-time setup:

- On npmjs.com, add a trusted publisher to `@freesialuo/contrib-scout`: GitHub Actions, this repository, workflow `release.yml`, environment `npm`.
- In the repository settings, create an environment named `npm` with required reviewers, and set the variable `NPM_TRUSTED_PUBLISHING_ENABLED` to `true`.
- Leave the workflow permissions default read-only. Only the **draft** and **release** jobs request `contents: write`, and only **publish** requests `id-token: write`.

To cut a release:

1. Set `[workspace.package].version` in `Cargo.toml` and `"version"` in `npm/package.json` to the same SemVer string. Refresh `Cargo.lock` (`cargo check --workspace`), run the checks above, and run `node scripts/npm-release.mjs validate vX.Y.Z`.
2. Create a signed commit and a matching signed tag, then push both: `git tag -s vX.Y.Z -m "vX.Y.Z"` and `git push origin main vX.Y.Z`.
3. Approve the `npm` deployment when the run asks for it.

## License

Licensed under either the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
