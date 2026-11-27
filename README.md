# contrib-scout

A command-line tool and Rust library that helps many AI agents find open-source issues worth contributing to, without each agent hammering the GitHub API on its own.

> [!NOTE]
> `contrib-scout` is not affiliated with or endorsed by GitHub. It is **read-only towards GitHub**: it never comments, opens pull requests, assigns, or claims anything on GitHub. Claims, marks, and history live only in a local database.

> [!IMPORTANT]
> `contrib-scout` **never judges a project's AI policy itself.** It fetches the raw policy text, stores the marks that agents write after reading that text, and repeats those marks as reminders. Whether a project accepts AI-assisted contributions is always decided by the agent (or human) who read the source.

## Why

When several agents look for contributions in parallel, each one tends to re-search GitHub, re-read the same `CONTRIBUTING.md`, and re-discover that a project bans AI-generated pull requests. `contrib-scout` gives them one shared memory:

- **One shared local database.** Every agent on the machine uses the same HTTP cache, GitHub quota, and claim table, so a key is fetched once even when two processes ask at the same moment.
- **Cheap discovery.** `discover` runs cheapest checks first and mostly from cache; `candidates` ranks issues with zero requests.
- **AI-policy marks written by agents.** An agent reads a project's raw policy files and records its reading with evidence. From then on, every command that touches that repository (or organization) repeats the mark as a reminder.
- **Issue marks.** An agent can mark an issue `skip` with a reason so later agents don't read it again, or `shortlist` a good one.
- **Live gates.** `gate start` and `gate pr` always check GitHub live and fail closed: they block on an unmarked policy, a ban, a skipped issue, another agent's claim, or an existing PR.

## Installation

`contrib-scout` talks to GitHub through the [GitHub CLI](https://cli.github.com). **It requires an authenticated `gh`** (`gh auth login`, check with `gh auth status`).

### From npm (recommended)

With Node.js 22 or newer:

```sh
npm install -g @freesialuo/contrib-scout
contrib-scout --help
```

Or run it without installing: `npx @freesialuo/contrib-scout --help`. The package bundles prebuilt binaries for macOS (Apple Silicon and Intel), Linux glibc (x64 and arm64, Ubuntu 22.04 baseline), and Windows x64. The same binaries are attached to each [GitHub Release](https://github.com/freesialuo/contrib-scout/releases).

### From source (Rust)

Building from source requires Rust 1.95 or newer (check with `rustc --version`; update with `rustup update stable`).

```sh
git clone https://github.com/freesialuo/contrib-scout.git
cd contrib-scout
cargo install --path crates/contrib-scout
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

## Commands

| Group | Commands | Purpose |
|---|---|---|
| Discovery | `discover`, `candidates`, `repo show` | Find and rank repositories and issues |
| AI policy | `policy files`, `policy set`, `policy show`, `policy unset`, `policy due` | Read raw policy text and record marks |
| Issues | `issue show`, `issue mark`, `issue unmark`, `issue marks` | Read issues and record skip or shortlist marks |
| Gates | `gate start`, `gate pr`, `claim list`, `claim renew`, `claim release` | Live checks before starting work and before opening a PR |
| Index | `track`, `sync`, `quota`, `cache`, `config` | Keep the shared index fresh and inspect usage |

Pass `--json` for a machine-readable `contrib-scout/v1` envelope; every command that touches a repository fills `meta.reminders` with its policy mark. Run `contrib-scout <command> --help` for options.

## Documentation

- [CLI reference](docs/cli.md): every command and option, the JSON envelope, gate verdicts, and exit codes.
- [Running contrib-scout](docs/operations.md): scheduling a periodic sync, where data is stored, and what contrib-scout does not do.
- [Releasing](docs/releasing.md): the GitHub and npm release pipeline.
- [Design](docs/design.md) (Chinese): architecture, data model, and the reasoning behind marks and gates.
- [Agent skill](skills/contrib-scout/SKILL.md): tells Claude agents how to drive `contrib-scout`.

## Development

The Cargo workspace separates the library [`crates/contrib-scout-core`](crates/contrib-scout-core) from the CLI [`crates/contrib-scout`](crates/contrib-scout). All tests run offline against a fixture transport; one live smoke test is `#[ignore]`d.

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
node --test scripts/*.test.mjs
```

Changes to the `contrib-scout/v1` JSON structure should stay backwards compatible for scripts and the skill.

## License

Licensed under either the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
