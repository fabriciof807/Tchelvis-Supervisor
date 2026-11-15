# Releasing Tchelvis Supervisor

Releases are built and published automatically by [`.github/workflows/release.yml`](../.github/workflows/release.yml).

## Nightly releases

Every push to `main` (after `cargo test --locked --quiet` passes):
1. Builds static `musl` binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` with `TCHELVIS_CHANNEL=nightly` and `TCHELVIS_COMMIT=<short-sha>`.
2. Packages `tchelvis-nightly-<target>.tar.gz` (containing `tchelvis`, `LICENSE-MIT`, `LICENSE-APACHE`, `README.md` at the archive root) and `tchelvis-nightly-<target>.tar.gz.sha256`, plus a combined `SHA256SUMS`.
3. Recreates the `nightly` tag and GitHub prerelease pointing to `$GITHUB_SHA`, with a release body containing `commit <short-sha> · <YYYY-MM-DD>`.

## Cutting a stable release (`vX.Y.Z`)

1. **Ensure `main` is clean and tests pass:**
   ```bash
   cargo test --locked --quiet
   ```
2. **Update `CHANGELOG.md`:**
   - Move the items under `## [Unreleased]` into a new `## [X.Y.Z] - YYYY-MM-DD` section.
   - Update the comparison links at the bottom of `CHANGELOG.md`.
   - The release workflow extracts the `## [X.Y.Z]` section verbatim as the GitHub Release notes.
3. **Bump the version in `Cargo.toml`:**
   - Set `version = "X.Y.Z"` in `[package]` and run `cargo check` (or `cargo test --locked`) so `Cargo.lock` updates if needed.
4. **Commit the release prep:**
   ```bash
   git add CHANGELOG.md Cargo.toml Cargo.lock
   git commit -m "chore(release): vX.Y.Z"
   git push origin main
   ```
5. **Tag and push the tag:**
   ```bash
   git tag -a "vX.Y.Z" -m "vX.Y.Z"
   git push origin "vX.Y.Z"
   ```
6. **Verify:**
   - Watch the `Release` workflow in GitHub Actions finish `test`, `build`, and `publish`.
   - Confirm the `vX.Y.Z` GitHub Release has both `.tar.gz` archives, both `.sha256` files, `SHA256SUMS`, and the extracted changelog notes.
