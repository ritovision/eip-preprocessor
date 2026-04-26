build-eips
==========

Build system for linting and rendering Ethereum Improvement Proposals ([EIPs] /
[ERCs]).

## Prerequisites

`build-eips` requires a few runtime dependencies, available from wherever you
get your software:

- git
- libgit2
- openssl
- [zola](https://github.com/getzola/zola/tree/next)[^1]

[^1]: Requires at least commit [`ead17d0a3`] for full functionality.

[`ead17d0a3`]: https://github.com/getzola/zola/commit/ead17d0a3a20bfb67043a076c061b35ae6b6ddea

## Installation

### Pre-compiled Binaries

Pre-compiled binaries for Ubuntu, Windows, and macOS are available from
[GitHub Releases].

[GitHub Releases]: https://github.com/ethereum/build-eips/releases

### From Source

If you're feeling particularly adventurous, you can install the latest version
of `build-eips` like so:

```bash
cargo install --git https://github.com/ethereum/build-eips.git
```

[EIPs]: https://github.com/ethereum/EIPs/
[ERCs]: https://github.com/ethereum/ERCs/

## Usage

1. Clone either [`ethereum/EIPs`] or [`ethereum/ERCs`], and change directory
   into it.
1. Modify whatever proposal you'd like.
1. Run the site commands you need:
   - `build-eips check` to run the runtime site verification path
   - `build-eips build` to create an on-disk bundle of HTML
   - `build-eips serve` to launch the runtime dev server locally
   - `build-eips preview` to serve the last built output without rebuilding
1. Run explicit editorial validation when you need proposal-targeted `eipw`
   checks.

## Workspace Bootstrap

If `build-eips` is already installed, you can bootstrap a local multi-repo
workspace from inside `EIPs/` or `ERCs/`:

```bash
build-eips workspace init /work/EIPs-project
```

By default this:

- clones any missing declared sibling content repos
- clones `theme`
- creates `.local-build/`
- writes `.build-eips.toml`

For proposal-family template work, you can additionally clone `template`:

```bash
build-eips workspace init /work/EIPs-project --template
```

For platform development, you can additionally clone `preprocessor` and `eipw`:

```bash
build-eips workspace init /work/EIPs-project --platform-dev
```

The optional `--template` and `--platform-dev` flags can be combined when both
sets of optional repositories are needed.

Validate the workspace bootstrap at any point with:

```bash
build-eips workspace doctor
```

After bootstrap, direct `build-eips` commands can run from inside `EIPs/` or
`ERCs/` without repeating local path flags:

```bash
cd /work/EIPs-project/EIPs
build-eips check
build-eips build
build-eips serve
```

The generated starter config contains local workspace preferences for
`[server]` and `[site]`. Command behavior is built in. Workspace build artifacts
use `.local-build/<repo_id>` by convention; pass `--build-root <path>` for a
one-off output-location override.

Tracked active-repo metadata lives separately in `.build-eips.repo.toml` when a
repo provides one. That manifest owns the repo identity, environment URLs, and
declared sibling topology; `.build-eips.toml` remains workspace-local execution
config. During the migration window, current `EIPs` and `ERCs` checkouts without
that manifest still use the legacy identity fallback.

## Local Site Commands

Plain site commands are local-first:

- `build-eips check`
- `build-eips build`
- `build-eips serve`

They use the workspace-local `theme/`, workspace-local sibling repos, tracked
working-tree changes from the active repo, and staging environment endpoints.
Use `--clean` on these commands when you want to ignore tracked working-tree
changes in the active repo:

```bash
build-eips check --clean
build-eips build --clean
build-eips serve --clean
```

`--clean` keeps workspace-local theme and sibling sources. Use explicit
environment commands when you need remote sources and a clean active repo:

```bash
build-eips --staging build
build-eips --production build
build-eips parity build
```

## Source Mode Overrides

`build-eips parity build|serve|check` is the built-in remote clean
staging/parity path.

### Explicit overrides

Use these flags to override the built-in source and output-location choices
directly:

- `--staging` / `--production`
- `--remote-theme`
- `--remote-sibling-repo`
- `--build-root <path>`

Workspace-local sources come from the standard workspace layout. The local theme
is `workspace/theme`, and local sibling repos are `workspace/<sibling_repo_id>`
from the active repo manifest. Use `--remote-theme` or `--remote-sibling-repo`
when you need to force remote sources for a single command.

Example:

```bash
build-eips \
  -C /work/EIPs-project/EIPs \
  --staging \
  --remote-theme \
  --remote-sibling-repo \
  --build-root /work/EIPs-project/.local-build/EIPs \
  check
```

## Dirty Mode

This compatibility heading now describes the default local development path.
Plain `check`, `build`, and `serve` include tracked active-repo edits by default.
Only the active content repo is materialized this way; sibling repos and theme
still come from the workspace layout unless you pass `--remote-theme` or
`--remote-sibling-repo`.

Local development limits:

- untracked files in the active content repo are ignored
- tracked deletions are mirrored into the materialized repo, but served route
  invalidation under Zola fast serve remains best-effort
- `--clean` ignores tracked active-repo edits for one command

## Editorial Commands

Use the explicit editorial command group when you want targeted `eipw`
validation:

```bash
build-eips editorial lint content/07949.md
build-eips editorial lint --working-tree
build-eips editorial lint --against-upstream --format github
build-eips editorial build --batch /work/EIPs-project/editor-batch.txt
```

Selector modes are mutually exclusive:

- explicit repo-relative proposal paths
- `--batch <path>` with one repo-relative proposal path per line
- `--working-tree` for tracked dirty proposal files
- `--against-upstream` for PR-style merge-base selection

`editorial build` runs targeted editorial validation first, then reuses the
runtime `check` path.

## Serve And Preview

Local serving keeps two distinct modes:

- `build-eips serve` for the runtime dev loop
- `build-eips preview` for serving already-built static output

`build-eips preview` serves the resolved output directory for the active repo
without invoking Zola, preprocessing markdown, or rebuilding anything.
If the output directory does not exist yet, it fails and tells you to run
`build-eips build` first.

The workspace config `[server]` table controls the local bind address for both
`serve` and `preview`; the default is `127.0.0.1:1111`. Per-command `--host`
and `--port` flags override that config for one run. These settings do not
change build base URLs.

The workspace config `[site].base_url` value is a local rendered-site URL
default. Starter configs set it to `http://127.0.0.1:1111`; if you change
`[server].port`, update `[site].base_url` too when generated links should match
the local server. Explicit environment and parity commands ignore
`[site].base_url`, including `--staging`, `--production`, and `parity`.
Per-command `--base-url` on `build` or `serve` is a one-run override and wins
even with staging or parity.
`preview` serves existing output, so build with `--base-url` first when previewed
HTML should contain a different local link target.

[`ethereum/EIPs`]: https://github.com/ethereum/EIPs/
[`ethereum/ERCs`]: https://github.com/ethereum/ERCs/
