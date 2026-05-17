# Gentoo overlay new packages to RSS

Generate an RSS feed from a Gentoo overlay git repository. The feed contains
packages whose `category/package/metadata.xml` file was added in git history.
Deleted packages and ebuild version updates are ignored.

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

Use `--include-root` only if the repository root commit should be treated as a
source of new package events. Most overlays should leave it disabled.

## GitHub Pages

`examples/github-pages.yml` is a workflow for overlay repositories. It checks
out full git history, builds this generator, writes `public/<repo>.rss`, and
publishes the directory through GitHub Pages.

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

The binary has no Rust crate dependencies. It requires `git` at runtime.
