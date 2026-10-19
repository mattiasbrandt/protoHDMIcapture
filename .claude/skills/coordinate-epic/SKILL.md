---
name: coordinate-epic
description: Run an epic's tickets as coordinator and critic - slice them file-disjoint, dispatch parallel workers in worktrees, review each slice, integrate onto main one at a time, keep the issues current. Use when the user asks to pick up an epic, dispatch workers for sub-issues, or run tickets in parallel.
---

You are the coordinator: assignment, integration and ruthless review. Workers
implement; you do not. Read the epic and every sub-issue you will dispatch in
full first, then `CLAUDE.md`'s pointers. Sized for this project: one user, a
hobby viewer, plain commits on `main`, push only when told.

## Before slicing

- **Open design decisions mean you are early.** A ticket whose interface or
  scope is still a question gets the `grilling` skill first; its decisions go
  into the ticket body. A worker builds a decision, never takes one.
- **Every issue action goes through the `github-issues` skill.**
- **A ticket carries what the worker needs, not a pointer to it.** The numbers,
  the strings verbatim, the commit or file that shows the pattern. A worker in
  a worktree has the ticket and the repo, not this session's context. A thin
  ticket is your defect: repair the body before dispatch.

## Slicing

- **File-disjoint wherever possible.** Each worker owns a set of files. Shared
  files (`viewer/src/app.rs`, `viewer/src/main.rs`, `viewer/Cargo.toml` are the
  usual ones) are listed in every brief with the *shape* of the allowed edit:
  "one `mod` line, one CLI arm, one key binding; no restructuring".
- **Excluded from dispatch:** anything that needs the card, a monitor, the
  Windows VM or a decision. Name each and its blocker in the kickoff report.
- **Sub-issues are minted at slicing time.** A small find in flight rides along
  in the ticket that owns those files. Workers never create issues; minting is
  your call, and only for a find that needs a decision, its own verification,
  or files no open ticket owns.

## Assignment

1. **One worktree per ticket, with the tool:** `tools/epic-worktree <issue> <slug>`.
   It branches off `main`, checks out under `.claude/worktrees/`, copies
   `viewer/target` by reflink (a plain copy where there is none), and prints
   the path. The branch stays local:
   `gh issue develop` would create it on GitHub, which is a push.
2. **Render each brief from [worker-brief.md](worker-brief.md)**, filling its
   placeholders; the ticket-specific knowledge stays in the ticket.
3. **Dispatch every worker in one message:** `Agent` with
   `subagent_type: "fork"` and `run_in_background: true`. A fork inherits this
   session's context, so the brief stays short. You are notified when each one
   finishes; no polling.

## Critic protocol (every slice, before it reaches `main`)

A worker's report is a claim. In its worktree, yourself:

1. **Read the production diff:** `git diff main...<branch>`. Ask: is the new
   code wired in and called, or added beside what it replaces? Were the old
   rules deleted, or are there now two copies? Were call sites missed? Does it
   do what the ticket says? Tests are scaffolding; the source is what ships.
2. **Check the edits to shared files** against the shape the brief allowed.
3. **Check the tests can fail:** each new test the worker says fails with the
   bug put back. Spot-check one yourself where it is cheap.
4. **Verify the tree, not the narrative:** `git log main..<branch>`, a clean
   `git status`, the new symbols on disk.
5. **Reject with concrete defects** (`file:line`, what is wrong, the bar),
   back to the same worker through `SendMessage`. A second rejection with the
   same failure: stop sending prose; supply a check, take the design back, or
   re-slice.

## Integration (serial, you alone)

1. **Smallest surface first**, so the larger slices rebase onto the settled ones.
2. **Rebase with the check on every commit:** in the worktree,
   `git rebase --exec tools/check main`. A rebase skips the pre-commit hook, and
   a resolved conflict can leave a commit that does not build (#13's wizard
   commit, rebased onto #17, lacked the `pixels` field `abe2b38` added). Fix
   such a commit in place, not with a later fix-up.
3. **Fast-forward `main`:** `git merge --ff-only <branch>`. No merge commits.
4. **Cross-slice assumptions, after each merge.** List what each slice took as
   given and check it against what the other slices changed. A worker cannot
   see its neighbours: on #10's first wave, #16 assumed one viewer per process
   while #13 opened two windows in one (`a449237`), #13's picture check
   assumed MJPEG never arrives decoded while #17 decoded it (`10e0ac3`), and
   #13 offered NV12 that the capture code refused (`eca1f02`). Each was found
   only here or in review.
5. **`tools/check` on `main`** after the last merge of the wave.
6. **Clean up:** move anything the user needs out of the worktree first (a
   built `.exe`, an archive), then `git worktree remove <path>` and
   `git branch -d <branch>`. `-d` refuses an unmerged branch, which is the
   check you want. An abandoned slice's branch is unmerged by definition, and
   the guard hook blocks the forced delete: leave the branch, and give the
   user the delete command to run.
7. **Recommend a review** of the wave's range to the user (`/codex:adversarial-review --base <sha> <focus>`,
   or a reviewer subagent when asked). Findings are claims: verify each against
   the code, then ask the user which to fix before touching a file.

## Hardware and outward acts (the user's, never a worker's)

The card, the monitors and the Windows VM are shared with the user's own work
on this machine. Ask before every run on them; batch what a wave needs into one
request. Pushing, tagging and releases are the user's call each time.
A check a unit test or `tools/check` already proves is not repeated on the card.

## Status on the issues

- **One status comment per ticket, found by its marker**, not `--edit-last`:
  you and every worker post as the same GitHub user, so `--edit-last` can
  overwrite someone else's comment. A worker's starts with
  `<!-- worker-status-<issue> -->`; the epic's with `<!-- epic-status -->`.
  Find it with
  `gh api repos/{owner}/{repo}/issues/<n>/comments --jq '.[] | select(.body | startswith("<marker>")) | .id'`
  and update it with `gh api -X PATCH repos/{owner}/{repo}/issues/comments/<id> -F body=@<file>`.
- **Commits by hash only.** GitHub's links show what is pushed; push state,
  branch names and "not on main yet" go stale with the next rebase or push.
- **After each integration, refresh** the status comments of the tickets it
  touched and the epic's: what is on `main`, what is open, and each open item's
  blocker (the card, the user, another ticket).
- **Only you edit an issue body** and tick its boxes, after verifying the
  criterion yourself.

## Reporting to the user

Record first, escalate the residue. A finding goes onto the ticket that will
act on it before anything reaches the user. What reaches them, batched, each
with a recommended default:

- a decision that is theirs (scope, a term, priority between tickets);
- a blocker nobody can work around (a twice-rejected slice, a conflict you
  cannot resolve, a toolchain fault);
- an act that needs approval (hardware, pushing, anything outward-facing).

The test before raising a concern: if the user says nothing, does the work
still happen? If yes, it was routing; the ticket carries it.
