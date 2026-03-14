# Releasing Workflow

This project uses [release-plz](https://release-plz.dev/) to automate
versioning, changelog generation, and GitHub Release creation.

No packages are published to crates.io — this is a git-release-only setup.

## How release-plz Works

release-plz has a two-phase workflow. Both phases run as parallel jobs in
a single GitHub Actions workflow (`.github/workflows/release-plz.yml`),
triggered on every push to `main` and on manual `workflow_dispatch`.

### Phase 1: `release-plz release`

Checks whether the current `Cargo.toml` version has already been released:

- For `git_only = true` packages (like `unbrk-cli`): checks **git tags**
- For normal packages: checks the **crates.io registry**

If the version is unreleased, it:

1. Creates a git tag (e.g., `v0.2.0`)
2. Creates a GitHub Release with the changelog body
3. Runs `cargo publish` (skipped when `publish = false`)

If already released, it prints "nothing to release" and exits.

**This job does NOT modify any files** — it releases what's already in
the repo.

### Phase 2: `release-plz release-pr`

Analyzes conventional commits since the last release tag:

- If new commits exist: creates or updates a PR that bumps the version in
  `Cargo.toml` and updates `CHANGELOG.md`
- If no changes: prints "the repository is already up-to-date"

## The Release Cycle

```
  developer pushes code to main
    |
    v
  workflow fires (push to main)
    ├── release job: "is there an unreleased version?" → usually no, noop
    └── release-pr job: "are there unreleased commits?" → creates/updates PR
    |
    v
  maintainer reviews & merges the release PR
    |
    v
  workflow fires again (the merge is a push to main)
    ├── release job: "version X.Y.Z has no tag" → creates tag + GitHub Release
    └── release-pr job: "no new commits since bump" → "already up-to-date"
```

## Configuration

### `release-plz.toml`

```toml
[workspace]
repo_url = "https://github.com/ynezz/unbrk"
pr_labels = ["release"]
release_always = false          # only release when there are changes
semver_check = false            # no semver API checking
git_release_enable = false      # disabled at workspace level
git_tag_enable = false          # disabled at workspace level

[[package]]
name = "unbrk-cli"
changelog_path = "CHANGELOG.md"
changelog_include = ["unbrk-core"]  # include core changes in CLI changelog
git_only = true                 # version detection from git tags, not crates.io
publish = false                 # not published to crates.io
git_release_enable = true       # create GitHub Releases
git_tag_enable = true           # create git tags
git_tag_name = "v{{ version }}" # tag format: v0.1.0, v0.2.0, ...

[[package]]
name = "unbrk-core"
publish = false
release = false                 # excluded from release management

[[package]]
name = "xtask"
publish = false
release = false                 # excluded from release management
```

Key design decisions:

- Only `unbrk-cli` is released (it's the user-facing binary)
- `unbrk-core` changes are folded into the CLI changelog via
  `changelog_include`
- `git_only = true` means release-plz checks git tags (not crates.io) to
  determine if a version has been released
- Tags use the `v{{ version }}` format (e.g., `v0.2.0`)

### GitHub Actions Workflow

The workflow (`.github/workflows/release-plz.yml`) runs both jobs on every
push to `main`. Key details:

- `fetch-depth: 0` + `fetch-tags: true` on the release job checkout,
  because `git_only = true` needs full tag history for version detection
- `persist-credentials: false` for security — release-plz uses the
  explicit `--git-token` for GitHub API calls
- Prefers `RELEASE_PLZ_TOKEN` secret (PAT) over `GITHUB_TOKEN` so that
  created tags/releases can trigger downstream workflows (e.g., binary
  builds)

## Release Artifacts

Cross-platform binary artifacts are built by
`.github/workflows/dist.yml`, which uses `cargo-dist` as the packager and
attaches the resulting archives, checksum files, and installer scripts to
the published GitHub Release.

This workflow intentionally complements `release-plz` instead of letting
`cargo-dist` manage its own generated `release.yml`:

- `release-plz` already owns versioning, changelog generation, git tags,
  and GitHub Release creation for this repository
- `cargo-dist` only handles artifact planning/building/uploading
- the repository keeps the human-facing workflow name as `dist.yml`

`dist.yml` has two modes:

- On pull requests it runs `dist plan` against the current `unbrk-cli`
  version, so release breakage is caught before merge
- On `release.published` it builds the four primary targets:
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
  `x86_64-apple-darwin`, and `aarch64-apple-darwin`, then uploads the
  artifacts to the existing GitHub Release

The `cargo-dist` configuration lives in `dist-workspace.toml`. We keep the
workspace explicit:

- only `unbrk-cli` is distributed
- `unbrk-cli` explicitly sets `package.metadata.dist.dist = true` because
  the workspace inherits `publish = false`
- archives are `.tar.gz` on Unix and `.zip` on Windows
- shell and PowerShell installers are enabled
- SHA-256 checksums are generated alongside the archives
- GitHub Artifact Attestations are generated in the build jobs for the
  same files that are later uploaded to the GitHub Release

The attestation step uses `actions/attest-build-provenance@v3` with the
GitHub-documented permissions:

- `contents: read`
- `id-token: write`
- `attestations: write`

This keeps provenance attached to the job that actually built each set of
release files, instead of trying to attest them later from the upload job.

Attestations are available on public repositories for all current GitHub
plans. For private or internal repositories, GitHub requires Enterprise
Cloud.

To verify a downloaded release artifact locally, use GitHub CLI:

```bash
gh attestation verify ./unbrk-cli-x86_64-unknown-linux-gnu.tar.gz \
  -R ynezz/unbrk
```

Because the artifact workflow is triggered by `release-plz`-created
releases, `RELEASE_PLZ_TOKEN` is the recommended token. A release created
with the default `GITHUB_TOKEN` will not trigger downstream workflows such
as `dist.yml`.

## Manual Release (Current Process)

The automated `release-plz release-pr` job is currently blocked by an
upstream bug ([release-plz#2595](https://github.com/release-plz/release-plz/issues/2595))
where `cargo package` fails for workspaces with unpublished path
dependencies. Until that is fixed, releases are created manually:

```bash
# 1. Bump version in workspace root
#    Edit Cargo.toml: version = "X.Y.Z"

# 2. Update CHANGELOG.md with the new version section

# 3. Commit the version bump
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -s -m "chore: release vX.Y.Z"

# 4. Push, tag, and create the GitHub Release
git push
git tag vX.Y.Z
git push origin vX.Y.Z
gh release create vX.Y.Z --title "vX.Y.Z" --notes-file CHANGELOG.md
```

Once release-plz#2595 is fixed, the automated release-pr flow described
above will handle steps 1-3 automatically via PR.

## Troubleshooting

### release-plz release-pr fails with "cargo package failed"

This is the upstream bug. The error looks like:

```
error: failed to prepare local package for uploading
Caused by: no matching package named `unbrk-core` found
  location searched: crates.io index
```

`cargo package` tries to resolve the `unbrk-core` path dependency from
crates.io, which fails because it's not published there. Track
[release-plz#2595](https://github.com/release-plz/release-plz/issues/2595)
for a fix.

### "skipping release: current commit is not from a release PR"

The `release-plz release` job only creates releases when it detects the
current commit is from a merged release-plz PR. Manual version bumps
won't trigger it — use `gh release create` directly instead.

### Token permissions

The default `GITHUB_TOKEN` works for creating releases, but tags/releases
created with it **will not trigger other workflows** (GitHub's anti-cascade
rule). Set up a `RELEASE_PLZ_TOKEN` PAT secret if you need downstream
workflows (like binary builds) to trigger on release.
