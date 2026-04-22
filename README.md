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

- clones the missing sibling content repo
- clones `theme`
- creates `.local-build/`
- writes `.build-eips.toml`

For platform development, you can additionally clone `preprocessor` and `eipw`:

```bash
build-eips workspace init /work/EIPs-project --platform-dev
```

Validate the workspace bootstrap at any point with:

```bash
build-eips workspace doctor
```

After bootstrap, daily commands can run from inside `EIPs/` or `ERCs/` without
repeating local path flags:

```bash
cd /work/EIPs-project/EIPs
build-eips check
build-eips build
build-eips serve
```

The generated starter config uses a custom `local` profile as the default
workspace-local profile. The built-in `parity` and `dirty` profiles live in
Rust and do not appear in `.build-eips.toml`.

## Profiles And Overrides

The command surface has three profile-selection paths:

- no explicit selection, which uses `default_profile` when configured
- `build-eips --profile <name> ...`
- built-in profile aliases such as `build-eips parity build` and
  `build-eips dirty serve`

Built-in profile behavior:

- `parity` uses staging plus remote theme and sibling sources
- `dirty` uses staging plus local theme and sibling sources, and enables dirty
  mode

`dirty` is still selectable without `.build-eips.toml`, but it cannot infer
local workspace paths on its own. If you run `dirty` without workspace config,
either bootstrap the workspace first, pass explicit local paths, or force the
remote sources with `--remote-theme` and `--remote-sibling-repo`. That remote
override form is still distinct from `parity`, because `dirty` keeps
`allow_dirty = true`.

### Explicit overrides

Use these flags to override the selected profile directly:

- `--staging` / `--no-staging`
- `--allow-dirty` / `--no-allow-dirty`
- `--theme <path>` / `--remote-theme`
- `--sibling-repo <path>` / `--remote-sibling-repo`
- `--build-root <path>`
- `--config <path>`
- `--profile <name>`

Example:

```bash
build-eips \
  -C /work/EIPs-project/EIPs \
  --staging \
  --theme /work/EIPs-project/theme \
  --sibling-repo /work/EIPs-project/ERCs \
  --build-root /work/EIPs-project/.local-build/EIPs \
  check
```

The local theme override also reuses that checkout's `config/eipw.toml`.

## Dirty Mode

Dirty mode is the explicit local-only path for tracked working-tree changes in
the active content repo.

Use the built-in alias:

```bash
cd /work/EIPs-project/EIPs
build-eips dirty check
build-eips dirty build
build-eips dirty serve
```

Or enable the same behavior ad hoc:

```bash
build-eips --allow-dirty check
build-eips --allow-dirty build
build-eips --allow-dirty serve
```

`build-eips dirty serve` performs the expensive runtime preparation once at
startup, then watches the real active content repo and mirrors tracked changes
into the materialized repo that Zola is serving from.

Dirty-mode limits:

- dirty mode is opt-in and non-parity
- only the active content repo is materialized dirty
- sibling repo and theme still follow the selected profile or explicit overrides
- untracked files in the active content repo are ignored
- clean `build-eips serve` remains the clean runtime serve path
- tracked deletions are mirrored into the materialized repo, but served route
  invalidation under Zola fast serve remains best-effort

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

`build-eips preview` serves the resolved output directory for the active
profile without invoking Zola, preprocessing markdown, or rebuilding anything.
If the output directory does not exist yet, it fails and tells you to run
`build-eips build` first.

[`ethereum/EIPs`]: https://github.com/ethereum/EIPs/
[`ethereum/ERCs`]: https://github.com/ethereum/ERCs/
