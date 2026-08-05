# Releasing openairplay1 to crates.io

Releases are tag-driven: pushing a `vX.Y.Z` tag runs
[release.yml](../.github/workflows/release.yml), which checks the tag
against the workspace version, tests the workspace, and publishes all
four crates to crates.io using Trusted Publishing (GitHub OIDC — no
stored API token).

The four crates share one version number, and publish in dependency
order:

| Crate | Depends on |
|---|---|
| `openairplay1` | — |
| `openairplay1-dashboard-protocol` | — |
| `openairplay1-receiver` | `openairplay1`, `openairplay1-dashboard-protocol` |
| `openairplay1-dashboard` | `openairplay1-dashboard-protocol` |

Order matters: `cargo publish` verifies a crate by building it with its
dependencies resolved **from the registry, not the workspace**, so a
dependent cannot be published before what it depends on is up. The
workflow publishes in the order above and cargo waits for the index.

The publish step **tolerates a crate whose version is already on
crates.io**: cargo's "already exists on crates.io index" is treated as
success and the run continues. That makes a re-pushed tag safe after a
partial failure, and lets a crate that had to be published by hand pass
through untouched (see below).

Cargo is what gets asked, not the crates.io API — a plain `curl -I` to
`/api/v1/crates/…` answers **403**, and the first version of this
workflow used exactly that, so it decided nothing was published and
then died on the first crate that was.

## One-time setup, per crate

crates.io can only configure a trusted publisher for a crate that
already exists, so **the first version of any new crate has to be
published by hand**. This is Stefan's step — it needs a crates.io
login — and it is the one part of a release that is not automated.

1. Log in on crates.io with the GitHub account and create an API token
   (scope: `publish-new`), then locally, on the merged release commit:

   ```sh
   cargo login          # paste the token
   cargo publish -p <the-new-crate>          # in dependency order
   ```

2. On crates.io, for the new crate → Settings → Trusted Publishing, add
   a GitHub publisher:
   - repository: `st3fan/openairplay1`
   - workflow filename: `release.yml`
3. Revoke the API token. From here that crate releases through the tag
   workflow like the others.

Crates already set up this way: `openairplay1`,
`openairplay1-receiver`. Crates still needing it (never published):
`openairplay1-dashboard-protocol`, `openairplay1-dashboard`.

## Releasing a version

1. Make sure `main` is green (CI) and contains everything the release
   should have; hardware-affecting changes must have been validated
   against a real sender (see CLAUDE.md).
2. Bump `version` in **all four** `Cargo.toml` files and in the
   intra-workspace dependency requirements that name a version
   (`openairplay1-receiver` names both `openairplay1` and
   `openairplay1-dashboard-protocol`; `openairplay1-dashboard` names the
   protocol crate), update the README's `openairplay1 = "X.Y"` line and
   anything else behavior-related, and land it via a PR like any other
   change.
3. Verify the packages locally. Only the leaf crates can be checked
   before their dependencies are on the registry:

   ```sh
   cargo publish --dry-run -p openairplay1
   cargo publish --dry-run -p openairplay1-dashboard-protocol
   ```

   A dry run of `openairplay1-receiver` or `openairplay1-dashboard`
   fails until the version they depend on is published — that is
   expected, not a problem with the package.
4. If the release introduces a **new** crate, do the one-time setup
   above for it now, before tagging.
5. Tag the release commit on `main` and push the tag:

   ```sh
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```

6. Watch the Release workflow (`gh run watch`), then verify:
   - each crate's page on crates.io shows the new version;
   - <https://docs.rs/openairplay1> and
     <https://docs.rs/openairplay1-dashboard-protocol> build and render
     (a few minutes);
   - a scratch project with `openairplay1 = "X.Y"` compiles the
     crate-level example;
   - `cargo install openairplay1-receiver` and
     `cargo install openairplay1-dashboard` still work from the registry.

## If a release goes wrong

A published version is immutable — it cannot be replaced or deleted.
Fix forward: `cargo yank --version X.Y.Z -p <crate>` to stop new
projects resolving the bad version, then release a patch version.
Yanking never breaks existing `Cargo.lock` users.

A failed workflow run before the publish step is harmless: fix, delete
and re-push the tag. If the publish step failed midway, the crates that
made it up are already skipped by the version check, so re-pushing the
tag simply continues where it stopped — but **check crates.io first**,
because a version that made it up cannot be republished with different
contents.

## Autopilot

A release request from Stefan is standing permission to run every step
above once the version number is agreed — except the one-time setup's
token creation, `cargo publish` of a brand-new crate, and the
trusted-publisher configuration, which need a crates.io login and are
Stefan's.
