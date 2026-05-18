# Gentoo overlay new packages to RSS

Generate an RSS feed from a Gentoo overlay git repository. The feed contains
packages whose `category/package/metadata.xml` file was added in git history.
Deleted packages and ebuild version updates are ignored.

For each package item, the feed includes `DIST` file sizes from the package
`Manifest` when it exists.

The tool regenerates the whole RSS file on every run. It never appends to an
existing file, so repeated runs are deterministic and do not create duplicate
items.

## Why this exists

The original prototype cloned overlays and walked git history in Go. This
version is an independent Rust CLI that runs inside any existing Gentoo overlay
checkout and asks native `git` for history data. That keeps it small and fast,
and avoids hard-coded overlay names.

## Usage

From a Gentoo overlay checkout:

```sh
gentoo-overlay-new-packages-to-rss
```

By default it writes:

```text
public/<repo-name>.rss
```

The repo name is read from `profiles/repo_name`, with the git directory name as
a fallback.

Common options:

```sh
gentoo-overlay-new-packages-to-rss \
  --repo /path/to/overlay \
  --repo-url https://github.com/microcai/gentoo-zh \
  --output public/gentoo-zh.rss \
  --max-items 200
```

Add `--list-patches` to include patch filenames from each package's `files/`
directory when `.patch` or `.diff` files exist.

Add `--github-author-links` to use the GitHub commit API for author links when
the git author email is not a GitHub noreply address. Set `GITHUB_TOKEN` or
`GH_TOKEN` to avoid GitHub's low anonymous API rate limit. This optional mode
requires `curl` at runtime.

Author emails are included in item descriptions by default, because some RSS
readers hide the email from the RSS author field. Add `--no-author-email` if
your reader already shows author emails and you want to avoid duplication.

Use `--include-root` only if the repository root commit should be treated as a
source of new package events. Most overlays should leave it disabled.

## GitHub Pages

`examples/github-pages.yml` is a ready-to-use workflow for overlay repositories:
copy it to `.github/workflows/new-packages-rss.yml` without editing it. It
checks out full git history, downloads the generator release binary, writes
`public/<repo>.rss`, and publishes the directory through GitHub Pages.

On normal pushes, the example runs only when the pushed commits add a
`category/package/metadata.xml` file. Manual `workflow_dispatch` runs still
regenerate the full feed.

For `microcai/gentoo-zh`, the generated feed path is:

```text
public/gentoo-zh.rss
```

## Development

```sh
cargo fmt
cargo test
cargo run -- --repo /path/to/overlay --output /tmp/feed.rss
```

There is also an ignored integration test for a real overlay checkout:

```sh
cargo test --test real_overlay -- --ignored
GENTOO_OVERLAY_RSS_REAL_REPO=/path/to/overlay cargo test --test real_overlay -- --ignored
```

The binary includes its Rust dependencies at build time. It requires `git` at runtime.

## Releases

Push a version tag to publish the Linux binary:

```sh
git tag v0.6.0
git push origin v0.6.0
```

The release workflow uploads:

```text
gentoo-overlay-new-packages-to-rss-linux-x86_64
gentoo-overlay-new-packages-to-rss-linux-x86_64.sha256
gentoo-overlay-new-packages-to-rss-linux-arm64
gentoo-overlay-new-packages-to-rss-linux-arm64.sha256
```

This is LLM (gpt-5.5 xhigh) rewrite of my Go project https://gitlab.com/vitaly-zdanevich/gentoo-guru-new-packages-to-rss
