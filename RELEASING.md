# Releasing migratr

This is the procedure an agent follows to release migratr. It has no operator
step: no confirmation, no choice. Run the steps in order. On the first failure,
stop and report it as the Failure section says.

Config is `release.toml`. The mechanics are `scripts/release.sh` and
`scripts/sync-version.sh`, copied from runlegion/legion (legion 0.43.5, commit
58504b1c) with two patches: the version bump also moves the `version` pin on
every `path = "..."` dependency, and `--activate` is refused. Check their
behavior with `bash scripts/test-release.sh`.

Replace `X.Y.Z` with the new version, `<level>` with `patch`, `minor` or
`major`, `<pr>` with the release PR number, and `W` with the docs worktree path.

## Keeping state

A release can outlive one agent session. After every step that creates something
(the release issue, the release PR, the tag, the docs worktree, the shingle PR),
comment it on the release issue. To resume, read that issue's comments, check
what exists, and continue at the first step not done.

## 1. Preconditions

On `main`, in sync with origin, no uncommitted changes to tracked files.
Untracked files are ignored, as `release.sh`'s own guard ignores them.

```bash
git rev-parse --abbrev-ref HEAD                      # main
git fetch origin main && git status -sb              # no "ahead"/"behind"
git status --porcelain --untracked-files=no          # empty, or only CHANGELOG.md when resuming after step 2
git branch --list 'release/*'                        # empty
```

If any fails, stop and report which. Change nothing.

## 2. Changelog

Dispatch the `legion:changelog` agent for migratr. It prepends `## X.Y.Z` to
`CHANGELOG.md` with a line `<Patch|Minor|Major> release: <rationale>`. The bump
level is that word, lowercased. Current choice (Sean, 2026-10-07): the changelog
agent's rationale sets the bump level, with no operator; revisable.

Tell it to put the `<Patch|Minor|Major> release: <rationale>` line as the first
paragraph under the heading. Read the result back from `CHANGELOG.md`: `X.Y.Z` is
the first `## X.Y.Z` heading, and `<level>` is the word before `release:` in the
first paragraph under it, lowercased. If the agent failed, or the entry has no
such heading and line, stop and report it.

Leave the entry uncommitted. `release.sh` takes it into the release commit.

## 3. Release issue

```bash
gh issue create --repo rafters-studio/migratr \
  --title "chore(release): vX.Y.Z" \
  --body "migratr X.Y.Z is tagged, on crates.io, and on a GitHub release with four archives and checksums.txt"
```

The pr-write gate needs an issue to map. Note the issue number.

## 4. Stage the release commit

```bash
scripts/release.sh <level>
```

It runs the preflight commands from `release.toml`, bumps `Cargo.toml`
(`[workspace.package]` and the three path-dependency pins), refreshes
`Cargo.lock`, commits `chore(release): X.Y.Z` on `release/X.Y.Z`, and pushes that
branch. It refuses when the `## X.Y.Z` heading does not equal the version it
computes from `<level>`; stop and report its message.

## 5. Gates, PR, merge queue

On `release/X.Y.Z`, run `/legion:legion-simplify`, then `/legion:legion-pr-write`
(both are keyed to the release commit). pr-write runs
`legion pr write-check --repo migratr --issue <issue>` on a body file, BODY, kept
in a scratch directory outside the repo, that maps the release issue's criterion. Then open the PR with that body; its
output includes `created PR #<pr>`, which gives `<pr>`:

```bash
legion pr create --repo migratr --title "chore(release): vX.Y.Z" --head release/X.Y.Z --closes <issue> --body "$(cat BODY)"
legion pr merge --repo migratr --number <pr>
```

## 6. Tag

```bash
scripts/release.sh X.Y.Z --finish=<pr>
```

It waits for the merge, tags the merged release commit, and pushes the tag. The
tag runs `release.yml`: four archives, `checksums.txt`, a GitHub release, and the
four crates on crates.io.

## 7. Wait for the GitHub release

Poll up to 90 times, 20 seconds apart, until the latest release is `vX.Y.Z`.
This uses `gh`, not `curl`: legion's hook stops `curl` for operator approval,
which would stall the release.

```bash
for i in $(seq 90); do
  tag="$(gh release view --repo rafters-studio/migratr --json tagName --jq .tagName 2>/dev/null)"
  [ "$tag" = "vX.Y.Z" ] && break
  sleep 20
done
[ "$tag" = "vX.Y.Z" ] || { echo "release.yml has not published vX.Y.Z"; gh run list --repo rafters-studio/migratr --workflow release.yml --limit 1 --json url --jq '.[0].url'; exit 1; }
```

On timeout, report "release.yml has not published vX.Y.Z" with the Actions run
URL, and do not run the docs steps. This keeps the site from showing a version
before its release exists.

## 8. Docs worktree and changelog copy

`--docs-worktree` finds shingle through `legion watch list`, where it is
registered.

```bash
scripts/release.sh --docs-worktree        # prints W
cp CHANGELOG.md W/sites/smugglr.dev/src/migratr/CHANGELOG.md
```

Copy byte-for-byte, every release, so every release lands a shingle merge and
Workers Builds rebuilds with the new version.

Then dispatch `writer-smugglr` with the new `## X.Y.Z` entry and `W` only. It
edits `W/sites/smugglr.dev/src/pages/migratr/**` when user-facing behavior
changed, and may write nothing.

## 9. Docs PR

In `W`: commit with the message `docs(migratr): vX.Y.Z on smugglr.dev`. Look for
an open PR on the docs branch:

```bash
gh pr list --repo rafters-studio/shingle --head docs/migratr-current --state open --json number --jq '.[0].number'
```

If it prints a number, that is `<shingle-pr>`: push with
`legion push --repo shingle --branch docs/migratr-current`, skip to the poll
below, and create no issue. Otherwise create the shingle issue:

```bash
gh issue create --repo rafters-studio/shingle \
  --title "docs(migratr): vX.Y.Z on smugglr.dev" \
  --body "https://smugglr.dev/migratr/changelog/ shows the X.Y.Z entry"
```

Run `/legion:legion-simplify` and `/legion:legion-pr-write` in `W` (BODY maps
the shingle issue's criterion). Then push, open and merge:

```bash
legion push --repo shingle --branch docs/migratr-current
legion pr create --repo shingle --title "docs(migratr): vX.Y.Z on smugglr.dev" --head docs/migratr-current --closes <shingle-issue> --body "$(cat BODY)"
legion pr merge --repo shingle --number <shingle-pr>
```

On either path, poll up to 90 times, 20 seconds apart, until the PR state is
`MERGED`:

```bash
gh pr view <shingle-pr> --repo rafters-studio/shingle --json state --jq .state
```

## 10. Remove the docs worktree

```bash
scripts/release.sh --docs-worktree-done=W
```

## 11. Wait for the live site

Up to 30 times, 60 seconds apart, fetch https://smugglr.dev/migratr/changelog/
and https://smugglr.dev/migratr/ with the WebFetch tool until both show `X.Y.Z`.
Do not use `curl`: legion's hook stops it for operator approval.

## 12. Report

Report: the version, the release PR and the tagged sha, the docs PR, and the
live-site result.

## Failure

- Step 1 fails: stop and report which precondition. Nothing has changed.
- The `## X.Y.Z` heading disagrees with the version `release.sh` computes from
  `<level>`: it refuses at its header check. Stop and report both versions.
- Release-side failures (queue ejection, merge timeout, tag failure): report
  `release.sh`'s message verbatim and stop.
- A release-side gate fails (simplify or pr-write in step 5): stop and report
  the gate's output, the release issue and the pushed `release/X.Y.Z` branch.
  `release.sh` refuses to stage again while that branch exists, so a later run
  first deletes the unmerged branch and reuses or closes the issue.
- Step 7 times out: report "release.yml has not published vX.Y.Z" with the
  Actions run URL. The docs steps do not run.
- Docs-step failures (worktree refused, shingle gates fail, PR ejected, merge
  refused or timed out, live check timed out) do not undo the release. Report
  `INCOMPLETE: docs`, naming the step, the shingle PR and the retained
  worktree. The next release's whole-file copy of `CHANGELOG.md` carries the
  missed entry.
