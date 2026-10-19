---
status: accepted
---

# protoHDMIcapture legacy is kept, best effort, not retired

Decided 2026-10-19 (#10, in the user's grilling). Epic #10 planned to retire
view.sh + mpv once protoHDMIcapture had parity. Instead, protoHDMIcapture
legacy stays in the repository as a Linux-only alternative, on a
best-effort basis: it reads the same saved settings, `capture-cards/` and
`sources/`, but nothing in protoHDMIcapture depends on its code, no check
holds it to parity, and it may lag behind or break. It moves to `legacy/`,
and the settings table it shared moves out of its `lib/settings.sh` into
`settings.rows`, which protoHDMIcapture owns and legacy reads.

## Considered options

- **Retire it** (the plan until now): one product, but the user keeps the
  mpv-based edition as an alternative.
- **Frozen, kept working**: parity checks stay, so every settings change
  still costs both editions.
- **Full parity**: every change lands twice.
- **A separate repository**: the shared settings file, `capture-cards/`
  and `sources/` would then live in two places.

## Consequences

- An architecture review does not propose retiring legacy, nor keeping it
  in step; a change to protoHDMIcapture need not touch it.
- Legacy keeps lint only (shellcheck, the syntax checks); its harnesses
  move to `legacy/tests/`, run by hand.
