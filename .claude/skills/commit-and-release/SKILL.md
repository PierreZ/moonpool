---
name: commit-and-release
description: Write moonpool commits and understand its release flow - Conventional Commits with the crate as scope (<type>(<crate>): <description>), release-plz with one version_group across the published crates and a CHANGELOG.md per crate, draft release PRs labelled release, the no-backward-compatibility and no-deprecation policy, and which crates publish. Use when committing, when asked what the commit message should be, when a change removes or renames a public API, or when a release PR appears.
disable-model-invocation: false
---

# Commit and release

## Commit messages

Conventional Commits with the crate as the scope:

```
<type>(<crate>): <description>
```

Types: `fix`, `feat`, `build`, `chore`, `ci`, `docs`, `style`, `refactor`,
`perf`, `test`. The scope is the crate short name (`moonpool-sim`,
`moonpool-core`, `moonpool-hyper`, `moonpool-explorer`, `moonpool-assertions`,
`moonpool-buggify`, `moonpool-prometheus`, `moonpool-calibrate`, `xtask`,
`book`); a workspace-wide change may omit it. release-plz parses the type to
group the changelog and to pick the bump (`features_always_increment_minor`:
a `feat` bumps the minor even at 0.x), so the type is not decoration.

Examples:

```
feat(moonpool-sim): in-flight faults and end-to-end TCP flow control
fix(moonpool-core): route select! offsets through the sim stream
docs(book): renumber part III after the metrics chapter
```

The body says what changed and why, names the seeds or tests that proved it,
and lists the book chapters touched. Do not put a model name or a session
identifier in a commit or PR.

## Compatibility policy

The library is not in external use beyond the projects that pin it by git
rev. Prefer replacing code over maintaining backward compatibility, and
delete old APIs when replacing them; there is no deprecation cycle. A
removed or renamed public item still needs its book mentions updated
(`grep -rn <name> book/src`) and a changelog-visible commit type.

## Release flow (`release-plz.toml`)

- `release_always = false`: a release happens only by merging the release PR
  release-plz opens (draft, labelled `release`) on pushes to `main`.
- Published crates share `version_group = "moonpool"` and bump together;
  each has its own `changelog_path` (`crates/<crate>/CHANGELOG.md`, present
  today for `moonpool`, `moonpool-core`, `moonpool-sim`, `moonpool-assertions`,
  `moonpool-explorer`). `xtask`, `moonpool-sim-examples` and the wasm demo are
  not published.
- `semver_check = false` (pre-1.0), `dependencies_update = true`, git releases
  are created as drafts.
- Never hand-edit a `CHANGELOG.md` for a normal change; release-plz writes it
  from the commit types. Hand edits are for correcting a released entry.

Before pushing: `/validate`. A release PR is merged only when CI is green on
the release commit itself, because it bumps every crate's version.
