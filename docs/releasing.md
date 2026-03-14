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

## Manual Release

To trigger a release manually (e.g., after fixing a failed release):

1. Go to **Actions** > **release-plz** workflow
2. Click **Run workflow** on the `main` branch

This runs both jobs. If the current version has no tag, the release job
will create it.

## Troubleshooting

### "nothing to release"

The release job found no unreleased versions. Common causes:

- The version in `Cargo.toml` already has a matching git tag
- The release PR hasn't been merged yet (version not bumped)
- Missing `fetch-tags: true` in checkout (release-plz can't see tags)

### Release PR doesn't bump version

For initial releases where the version is already set in `Cargo.toml`
(e.g., `0.1.0`), release-plz may only add a changelog without bumping
the version. This is expected — the next release PR will bump to `0.2.0`.

### Token permissions

The default `GITHUB_TOKEN` works for creating releases, but tags/releases
created with it **will not trigger other workflows** (GitHub's anti-cascade
rule). Set up a `RELEASE_PLZ_TOKEN` PAT secret if you need downstream
workflows (like binary builds) to trigger on release.
