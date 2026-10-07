# Releasing migratr

This is the procedure an agent follows to release migratr. It has no operator
step: no confirmation, no choice. Run the steps in order. On the first failure,
stop and report it as the Failure section says.

Config is `release.toml`. The mechanics are `scripts/release.sh` and
`scripts/sync-version.sh`, copied from runlegion/legion (`scripts/release.sh` at
5a717f08) with two patches: the version bump also moves the `version` pin on
every `path = "..."` dependency, and `--activate` is refused. Check their
behavior with `bash scripts/test-release.sh`.

Replace `X.Y.Z` with the new version, `<level>` with `patch`, `minor` or
`major`, `<pr>` with the release PR number, and `W` with the docs worktree path.

## 1. Preconditions

On `main`, in sync with origin, clean tree.

```bash
git rev-parse --abbrev-ref HEAD            # main
git fetch origin main && git status -sb    # no "ahead"/"behind"
git status --porcelain                     # empty
```

If any fails, stop and report which. Change nothing.

## 2. Changelog

Dispatch the `legion:changelog` agent for migratr. It prepends `## X.Y.Z` to
`CHANGELOG.md` with a line `<Patch|Minor|Major> release: <rationale>`. The bump
level is that word, lowercased. Current choice (Sean, 2026-10-07): the changelog
agent's rationale sets the bump level, with no operator; revisable.

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
branch. Add `--dry-run` first to see the steps with nothing mutated.

## 5. Gates, PR, merge queue

On `release/X.Y.Z`, run `/legion:legion-simplify`, then `/legion:legion-pr-write`
(both are keyed to the release commit). Then:

```bash
legion pr create --repo migratr --title "chore(release): vX.Y.Z" --head release/X.Y.Z --closes <issue>
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

Poll up to 90 times, 20 seconds apart, until the redirect target ends in
`/vX.Y.Z`:

```bash
for i in $(seq 90); do
  url="$(curl -sIL -o /dev/null -w '%{url_effective}' https://github.com/rafters-studio/migratr/releases/latest)"
  case "$url" in */vX.Y.Z) break ;; esac
  sleep 20
done
case "$url" in */vX.Y.Z) ;; *) echo "release.yml has not published vX.Y.Z"; gh run list --repo rafters-studio/migratr --workflow release.yml --limit 1 --json url --jq '.[0].url'; exit 1 ;; esac
```

On timeout, report "release.yml has not published vX.Y.Z" with the Actions run
URL, and do not run the docs steps. This keeps the site from showing a version
before its release exists.

## 8. Docs worktree and changelog copy

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

In `W`: commit. Create the shingle issue:

```bash
gh issue create --repo rafters-studio/shingle \
  --title "docs(migratr): vX.Y.Z on smugglr.dev" \
  --body "https://smugglr.dev/migratr/changelog/ shows the X.Y.Z entry"
```

Run `/legion:legion-simplify` and `/legion:legion-pr-write` in `W`. Then:

```bash
legion pr create --repo shingle --head docs/migratr-current --closes <shingle-issue>
legion pr merge --repo shingle --number <shingle-pr>
```

If an open PR already exists on `docs/migratr-current`, push to it instead of
creating one. Then poll up to 90 times, 20 seconds apart, until the PR state is
`MERGED`:

```bash
gh pr view <shingle-pr> --repo rafters-studio/shingle --json state --jq .state
```

## 10. Remove the docs worktree

```bash
scripts/release.sh --docs-worktree-done=W
```

## 11. Wait for the live site

Poll up to 30 times, 20 seconds apart, until both pages contain `X.Y.Z`:

```bash
curl -s https://smugglr.dev/migratr/changelog/ | grep -q 'X.Y.Z'
curl -s https://smugglr.dev/migratr/ | grep -q 'X.Y.Z'
```

## 12. Report

Report: the version, the release PR and the tagged sha, the docs PR, and the
live-site result.

## Failure

- Step 1 fails: stop and report which precondition. Nothing has changed.
- The changelog header disagrees with the level: `release.sh` refuses at its
  `## <new>` header check. Stop and report both versions.
- Release-side failures (queue ejection, merge timeout, tag failure): report
  `release.sh`'s message verbatim and stop.
- Step 7 times out: report "release.yml has not published vX.Y.Z" with the
  Actions run URL. The docs steps do not run.
- Docs-step failures (worktree refused, shingle gates fail, PR ejected, merge
  refused or timed out, live check timed out) do not undo the release. Report
  `INCOMPLETE: docs`, naming the step, the shingle PR and the retained
  worktree. The next release's whole-file copy of `CHANGELOG.md` carries the
  missed entry.
