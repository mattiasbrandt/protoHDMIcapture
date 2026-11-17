# Contributing

Work is tracked in the [open issues](https://github.com/mattiasbrandt/protoHDMIcapture/issues);
[GLOSSARY.md](GLOSSARY.md) names the project's terms, and code, docs,
commits and issues use them.

## Workflow

1. Enable the hooks once per clone: `git config core.hooksPath .githooks`.
   `pre-commit` runs `tools/check` on what is staged; `commit-msg` checks
   the subject (below).
2. Commit straight to `main`, or open a PR. A PR is **squash-merged**: its
   title becomes the one commit on `main`, so the title follows the rules
   below and says what the change means for users.
3. CI runs `tools/check` (`check.yml`) and the commit check (`commits.yml`)
   on every push to `main` and every PR.
4. Before a run against a real card, read [docs/testing.md](docs/testing.md);
   [CODING_STANDARDS.md](CODING_STANDARDS.md) is the review checklist.

## Conventional Commits

Every commit subject on `main`, and every PR title, follows
[Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>(<scope>): <subject>
```

One space after the colon, and no trailing period. `tools/commit-subject`
is the rule, run by the `commit-msg` hook and by CI.

| Type | Use for | Version effect (from 1.0.0) | Before 1.0.0 |
|---|---|---|---|
| `feat` | New user-facing behaviour | MINOR | MINOR |
| `fix` | A bug fix users notice | PATCH | PATCH |
| `feat!` / `fix!` | Breaking change | MAJOR | MINOR |
| `perf` | Faster or lower latency, same behaviour | PATCH | PATCH |
| `refactor` | Restructure, same behaviour | none | none |
| `test` | Tests only | none | none |
| `docs` | README, docs, comments, agent instructions | none | none |
| `style` | Formatting only | none | none |
| `ci` | Workflows | none | none |
| `build` | Cargo, packaging, dependencies | none | none |
| `chore` | Housekeeping, release PRs | none | none |
| `revert` | Reverts an earlier commit | as the reverted change | |

A **breaking change** is anything that makes an existing setup stop
working without the user acting: a renamed or removed setting in the
settings file, a changed default that changes what a saved setting does,
a dropped distro or Windows version, a changed run-dir file another
program reads ([docs/run-dir.md](docs/run-dir.md)). Mark it with `!`
after the type or scope, and say what to do in a `BREAKING CHANGE:`
footer.

**Scopes** are optional and name the part touched, in the glossary's
words: `viewer`, `wizard`, `audio`, `capture`, `card`, `overlay`,
`settings`, `session`, `packaging`, `research`, `legacy`, `deps`,
`release`.

## Versioning and releases

The project follows [Semantic Versioning 2.0.0](https://semver.org/).
Before 1.0.0 a breaking change bumps MINOR, not MAJOR.

Releases need no hand ([`release-please.yml`](.github/workflows/release-please.yml)):

1. On every push to `main`, [release-please](https://github.com/googleapis/release-please)
   reads the commits since the last release. When one is a `feat`, `fix`,
   `perf` or breaking change, it opens or updates the release PR: the
   next version in `Cargo.toml` and `Cargo.lock`, and its `CHANGELOG.md`
   section.
2. The same run checks that PR with `tools/check`, squash-merges it, tags
   `v<version>` and creates the GitHub release as a draft.
3. [`release.yml`](.github/workflows/release.yml) builds and tests the
   Linux archive and the Windows MSI, uploads them with `SHA256SUMS`, and
   publishes the release.

Nobody edits the version, `CHANGELOG.md` or a tag by hand. A `v*` tag
pushed by hand still runs `release.yml`; its version must match
`Cargo.toml`. Run `release.yml` from the Actions tab to build and test
both packages without releasing anything.
