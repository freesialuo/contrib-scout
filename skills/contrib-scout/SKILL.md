---
name: contrib-scout
description: Find open-source GitHub issues that accept outside (AI-assisted) contributions, record AI-policy and issue marks for other agents, and pass the live gates before starting work and before opening a PR, using the contrib-scout CLI.
---

# Contributing with contrib-scout

Use this skill when asked to find open-source issues to work on, to check whether a project accepts AI-assisted contributions, or before starting work on or opening a PR for a GitHub issue that was found this way. Many agents share one local `contrib-scout` database: what you mark, they see.

`contrib-scout` is read-only towards GitHub. It never comments, applies, assigns, opens PRs, or claims anything on GitHub, and it never decides a project's AI policy itself. Never tell the user that the tool applied, commented, claimed remotely, or opened anything. Claims and marks are local.

## Setup

- Check `contrib-scout --version`. If missing: `npm install -g @freesialuo/contrib-scout` (Node.js 22+), or `cargo install --path crates/contrib-scout` (Rust 1.95+) from `https://github.com/freesialuo/contrib-scout`.
- It needs an authenticated GitHub CLI: if a command fails on auth, check `gh auth status` and ask the user to run `gh auth login`. Do not handle tokens yourself.
- Use a stable agent id for the whole task: `--agent <ID>` on every call, or `CONTRIB_SCOUT_AGENT`. Claims and mark history are recorded under it.
- **Always pass `--json`** and read the `contrib-scout/v1` envelope: `data`, `meta.reminders`, `meta.partial`, `meta.warnings`, `meta.quota`, and `error` on failure. Use `contrib-scout <command> --help` if a flag is unclear.

## Reminders come first

Every command that touches a repository returns `meta.reminders`. Read them before anything else:

- `kind: "marked"` — another agent already read the policy. Respect `stance` and `note`. If `review_due: true` and you are about to invest in this repository, re-read the policy (`policy files`) and mark it again.
- `kind: "unmarked"` — nobody has read this repository's policy yet. If the repository interests you, read and mark it (below) before doing anything else with it. `gate start` will block with `policy_unmarked` until you do.
- Repositories marked `bans` and issues marked `skip` are excluded from `discover` and `candidates` by default; the result says how many. Do not pass `--include-marked` to work around a ban.

## Workflow

1. **Find candidates (cached).**
   ```sh
   contrib-scout --json --agent ID candidates --lang rust --limit 20      # local only, zero requests
   contrib-scout --json --agent ID discover --lang rust --label "good first issue" --limit 20
   ```
   Prefer `candidates` first; use `discover` when the local index has too little. Shortlisted issues sort first and carry their note.

2. **Mark the AI policy of an interesting, unmarked repository.**
   ```sh
   contrib-scout --json --agent ID policy files owner/repo
   ```
   This returns raw files (org `{owner}/.github` first, then the repository) with path, permalink, and text; `truncated: true` means you only see part of it — follow the URL if the cut matters. **Read the text yourself.** Check also README sections and PR templates. Then record what you read:
   ```sh
   contrib-scout --json --agent ID policy set owner/repo --stance restricts \
     --evidence CONTRIBUTING.md:40-42 \
     --forbid-label "good first issue" \
     --scope pr_text=human_written \
     --require disclose_in_pr --require assisted_by_trailer \
     --note "AI code ok if disclosed; PR text must be human-written; no AI on good first issues"
   ```
   - `--stance`: `bans`, `restricts`, `disclose`, `allows`, `welcomes`, `none_found`. Choose from the text, not from keywords alone; prohibitive words that are not about AI do not make a ban.
   - Evidence is mandatory: the file path (or external URL, e.g. a project website) with lines. A range covers at most 3 lines (`PATH:40-42`); longer ranges are rejected, so quote the decisive lines and repeat `--evidence` for separate passages. For `none_found`, list the paths you checked.
   - Capture scope: `--forbid-label L` for "no AI on these issues"; `--scope needs_prior_approval=true` when maintainer approval on the issue is required first; `--scope pr_text=human_written`, `--scope comments=human_written`, `--scope autonomous_agents=forbidden` as stated.
   - Capture PR requirements with `--require`: `disclose_in_pr`, `assisted_by_trailer`, `human_written_description`, `human_written_replies`, `maintainer_approval_on_issue`, `label_pr=LABEL`.
   - If the policy lives in the organization's `.github` repository and covers all its projects, mark the org (`policy set owner ...`). A repository mark overrides an org mark.
   - The note is shown verbatim to every later agent: one precise sentence.
   - If the stance is `bans`, drop the repository.

3. **Read the issue; leave a mark either way.**
   ```sh
   contrib-scout --json --agent ID issue show 'owner/repo#123'
   ```
   If it is not worth doing, mark it so later agents skip it without re-reading:
   ```sh
   contrib-scout --json --agent ID issue mark 'owner/repo#123' --verdict skip --reason too_large \
     --note "Needs a storage redesign the maintainers have not agreed on"
   ```
   Reasons: `too_large`, `unclear`, `needs_special_env`, `out_of_scope`, `duplicate`, `maintainer_unresponsive`, `already_addressed`, `other`. If it is good but you will not take it now, use `--verdict shortlist` with a note. A reminder with `changed_since_mark: true` means the issue changed after it was marked; re-read it before relying on the old verdict.

4. **Gate before starting work.**
   ```sh
   contrib-scout --json --agent ID gate start 'owner/repo#123'
   ```
   Exit `0` / `verdict: "pass"`: you now hold a local claim (lease, renew with `claim renew`). Read the returned raw data (`raw.issue` body, recent comments, linked PRs; `raw.policy_mark`; the policy file list) and `observations` yourself. Observations never block, but act on them: `comment_claim` means a comment looks like someone taking the issue (a phrase heuristic; read it and back off if they are on it), `approval_required` means `gate pr` will need `--approval-url`, `mark_review_due` means re-read the policy and re-mark. `requirements` previews what the PR must do.
   Exit `20` / `verdict: "blocked"`: stop and act on `blocked_by`:
   - `policy_unmarked` → do step 2, then gate again.
   - `policy_bans` → drop the repository.
   - `issue_skipped` → another agent ruled it out; read the mark's reason. Only `issue unmark` if you have a concrete reason the verdict is wrong.
   - `forbidden_label` → the policy forbids AI on this kind of issue; drop it.
   - `claimed_by_other`, `assigned_to_other`, `pr_exists`, `issue_closed`, `repo_archived` → pick another issue.
   A blocked `gate start` does not take a claim.
   Exit `1` (`data: null`, `error.code` such as `network_unavailable` or `quota_reserved`): the live check failed and the gate failed closed (logged as verdict `error`). Do not start; retry later, never with `--offline`.

5. **Gate before opening a PR.**
   ```sh
   contrib-scout --json --agent ID gate pr 'owner/repo#123' --branch my-branch [--approval-url URL]
   ```
   Always pass `--branch` once your PR branch exists: your own linked PR (authored by the authenticated GitHub user, head branch equal to `--branch`) is then excluded from `pr_exists`, and the branch is recorded on your claim.
   - **Read the returned live contribution rules and the PR template in full.** They are the real gate. If they differ from the mark (new ban, new requirement, changed scope), stop, update the mark with `policy set`, and re-evaluate before going on.
   - Work through `checklist` and obey every item in `requirements`: disclose AI use in the PR description, add `Assisted-by:` trailers to commits, keep the description and replies human-written, get maintainer approval first, apply required labels, follow the PR template.
   - `blocked: human_required` → **stop and hand the work to the human**: the mark says the PR text, the description or the review replies must be written by a person (`pr_text` / `comments`: `human_written` or `forbidden`, or the `human_written_description` / `human_written_replies` requirements), autonomous agents are forbidden, or `code: forbidden` (an AI agent cannot write the code; this one already blocks `gate start`). Give them the branch, a summary, and the requirements; do not draft the PR text for them to paste unless they ask, and never open the PR yourself.
   - `claim_not_held` → run `gate start` again first.
   - Any reason from step 4 (`issue_skipped`, `pr_exists`, `claimed_by_other`, …) → handle it as in step 4.
   - `approval_missing` → the mark requires maintainer approval. Read the `approval_evidence` comments; if a maintainer clearly approved, gate again with `--approval-url <that comment's URL>` (the CLI records it, it does not verify it). Otherwise ask in the issue only if the human agrees, or stop.
   - Opening the PR is a separate action that the user must approve; `gate pr` only checks.

6. **Release what you will not finish:** `contrib-scout --json --agent ID claim release 'owner/repo#123'`, and leave a `skip` or `shortlist` mark with what you learned.

## Request economy

- Prefer the cache. Use `--offline` for reads when the cached data is enough (`candidates` is always local). Never loop `--fresh`; use it once, only when you need data newer than its TTL.
- Do not fetch `policy files` for every discovered repository — only for ones you intend to work on.
- `meta.partial: true` or non-empty `meta.warnings` mean some data is stale or missing; say so instead of presenting results as complete.
- Watch `meta.quota`. If quota is low or a command reports a quota error, stop making requests and continue from cache or later. Do not raise `--max-requests` repeatedly to push through.
- The scheduled `sync --all-tracked` refreshes the index; agents normally should not run broad syncs themselves. Use `track add owner/repo` to include a repository you care about.

## Exit codes

`0` success / pass; `10` new changes or candidates; `20` gate blocked; `1` error (gates fail closed); `2` usage error.
