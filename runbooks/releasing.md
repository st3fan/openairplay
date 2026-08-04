# Releasing openairplay1 to crates.io

Releases are tag-driven: pushing a `vX.Y.Z` tag runs
[release.yml](../.github/workflows/release.yml), which tests the
workspace and publishes the `openairplay1` library and then the
`openairplay1-receiver` binary to crates.io using Trusted Publishing
(GitHub OIDC — no stored API token). The library must publish first —
the binary depends on it, and its pre-publish verification build
resolves the library from the registry, not the workspace.

## One-time setup (first release only)

crates.io can only configure a trusted publisher for a crate that
already exists, so the very first publish is manual:

1. Log in on crates.io with the GitHub account and create an API token
   (scope: `publish-new`), then locally:

   ```sh
   cargo login          # paste the token
   cargo publish -p openairplay1
   cargo publish -p openairplay1-receiver   # after the library is up
   ```

2. On crates.io, for **each** crate (`openairplay1` and
   `openairplay1-receiver`) → Settings → Trusted Publishing, add a
   GitHub publisher:
   - repository: `st3fan/openairplay1`
   - workflow filename: `release.yml`
3. Revoke the API token — it is no longer needed. Subsequent releases
   go through the tag workflow only.

## Releasing a version

1. Make sure `main` is green (CI) and contains everything the release
   should have; hardware-affecting changes must have been validated
   against a real sender (see CLAUDE.md).
2. Bump `version` in `openairplay1/Cargo.toml` (and the `version` in
   `openairplay1-receiver/Cargo.toml`'s `openairplay1` dependency to
   match, plus its own `version` if the binary changed), update the
   README if behavior changed, and land that via a PR like any other
   change.
3. Verify the package locally:

   ```sh
   cargo publish --dry-run -p openairplay1
   ```

4. Tag the release commit on `main` and push the tag:

   ```sh
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```

5. Watch the Release workflow (`gh run watch`), then verify:
   - <https://crates.io/crates/openairplay1> shows the new version;
   - <https://docs.rs/openairplay1> builds and renders (a few minutes);
   - a scratch project with `openairplay1 = "X.Y"` compiles the
     crate-level example.

## If a release goes wrong

A published version is immutable — it cannot be replaced or deleted.
Fix forward: `cargo yank --version X.Y.Z -p openairplay1` to stop new
projects resolving the bad version, then release a patch version.
Yanking never breaks existing `Cargo.lock` users.

A failed workflow run before the publish step is harmless: fix, delete
and re-push the tag. If the publish step itself failed midway, check
crates.io — if the version made it up, do not re-push the tag; move to
the next patch version.

## Autopilot

A release request from Stefan is standing permission to run every step
above once the version number is agreed — except the one-time setup's
token creation and the trusted-publisher configuration, which happen
in the crates.io UI and are Stefan's.
