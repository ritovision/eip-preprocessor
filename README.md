build-eips
==========

Build system for linting and rendering Ethereum Improvement Proposals ([EIPs] / [ERCs]).

## Prerequisites

`build-eips` requires a few runtime dependencies, available from wherever you get your software:

- git
- libgit2
- openssl
- [zola](https://github.com/getzola/zola/tree/next)[^1]

[^1]: Requires at least commit [`ead17d0a3`] for full functionality.

[`ead17d0a3`]: https://github.com/getzola/zola/commit/ead17d0a3a20bfb67043a076c061b35ae6b6ddea

## Installation

### Pre-compiled Binaries

Pre-compiled binaries for Ubuntu, Windows, and macOS are available from [GitHub Releases].

[GitHub Releases]: https://github.com/ethereum/build-eips/releases

### From Source

If you're feeling particularly adventurous, you can install the latest version of `build-eips` like so:

```bash
cargo install --git https://github.com/ethereum/build-eips.git
```

[EIPs]: https://github.com/ethereum/EIPs/
[ERCs]: https://github.com/ethereum/ERCs/

## Preprocessor Development Setup

When working on `build-eips` itself, use the local contributor setup script instead of the public proposal-repo bootstrap scripts:

```bash
./scripts/dev-setup
```

The script runs `cargo build` and uses `preprocessor/target/debug/build-eips` for workspace setup, so local branch changes are what get exercised. It expects a sibling active proposal repo checkout, defaulting to `../EIPs`; override `WORKSPACE_ROOT` or `ACTIVE_REPO_ROOT` when your checkout layout differs. Run it from the preprocessor checkout; relative overrides are resolved from the invocation directory.

This setup mutates the local workspace root through `workspace init`, including creating `.build-eips.toml`, `.local-build/`, and cloning missing repositories. By default it passes `--platform-dev`, which adds `preprocessor` and `eipw`. Pass `--template` to the setup script when you also want `template`:

```bash
./scripts/dev-setup --template
```

Public proposal-repo setup scripts are different: they use an installed or released `build-eips`. Template public bootstrap remains gated on a manifest-aware release.

## Usage

1. Clone either [`ethereum/EIPs`] or [`ethereum/ERCs`], and change directory into it.
1. Modify whatever proposal you'd like.
1. Run the site commands you need:
   - `build-eips check` to run the runtime site verification path
   - `build-eips build` to create an on-disk bundle of HTML
   - `build-eips serve` to launch the runtime dev server locally
   - `build-eips preview` to serve the last built output without rebuilding
1. Run explicit editorial validation when you need proposal-targeted `eipw` checks.

[`ethereum/EIPs`]: https://github.com/ethereum/EIPs/
[`ethereum/ERCs`]: https://github.com/ethereum/ERCs/

## Workspace Bootstrap

If `build-eips` is already installed, you can bootstrap a local multi-repo workspace from inside `EIPs/` or `ERCs/`:

```bash
build-eips workspace init /work/EIPs-project
```

By default this:

- clones any missing declared sibling proposal repos
- clones `theme`
- creates `.local-build/`
- writes `.build-eips.toml`

To work on a bare, unpopulated proposal repo, you can additionally clone `template`:

```bash
build-eips workspace init /work/EIPs-project --template
```

For platform development, you can additionally clone `preprocessor` and `eipw`:

```bash
build-eips workspace init /work/EIPs-project --platform-dev
```

The optional `--template` and `--platform-dev` flags can be combined when both sets of optional repositories are needed.

Validate the workspace bootstrap at any point with:

```bash
build-eips workspace doctor
```

After bootstrap, direct `build-eips` commands can run from inside `EIPs/` or `ERCs/` without repeating local path flags:

```bash
cd /work/EIPs-project/EIPs
build-eips check
build-eips build
build-eips serve
```

The generated starter config contains local workspace preferences for `[server]`, `[site]`, and `[render]`. Command behavior is built in. Workspace build artifacts use `.local-build/<repo_id>` by convention; pass `--build-root <path>` for a one-off output-location override.

Tracked active-repo metadata lives separately in `.build-eips.repo.toml` when a repo provides one. That manifest owns the repo identity, environment URLs, and declared sibling topology; `.build-eips.toml` remains workspace-local execution config. During the migration window, current `EIPs` and `ERCs` checkouts without that manifest still use the legacy identity fallback.

## Local Site Commands

The main site commands use the local workspace in dirty mode by default to include tracked local edits:

* `build-eips check`
* `build-eips build`
* `build-eips serve`

By default, these commands use:

* `.build-eips.toml` for local workspace settings like `[site].base_url` and `[server].host` / `[server].port`
* tracked state from workspace-local `theme/`
* workspace-local sibling repos
* tracked working-tree changes from the active proposal repo

By default, `build-eips` prepares a disposable build repo under `.local-build/<repo_id>/repo`, then runs the requested check, build, or serve command from that prepared repo.

Sibling repos come from the workspace layout unless you pass `--remote-sibling-repo`.

Before running Zola, `build-eips` copies the tracked `workspace/theme` state into `.local-build/<repo_id>/repo/themes/eips-theme`.

### Dirty Local Mode Limits

* untracked files in the active proposal repo are ignored
* untracked files in `workspace/theme` are ignored
* new theme files must be staged with `git add` before local Zola commands see them
* tracked active-repo deletions are mirrored into the materialized repo
* tracked theme deletions are mirrored into the mounted theme directory
* served route invalidation under Zola fast serve remains best-effort

### Clean Local Commands

Use `--clean` when you want to ignore tracked working-tree changes in the active proposal repo:

```bash
build-eips check --clean
build-eips build --clean
build-eips serve --clean
```

`--clean` ignores tracked active-repo edits for one command, but still uses workspace-local sibling repos and the tracked state from workspace-local `theme/`.


### Targeted Local Rendering

Full local `build` and `serve` runs can take time because they process every proposal file. Targeted rendering reduces that time by rendering only the proposal files you choose while still including all non-proposal files. This is useful when testing a few proposals or iterating on theme changes.

Set the proposal numbers you want rendered in `.build-eips.toml`:

```toml
[render]
only = [555, 678]
```

Use the CLI `--only` when you want `build` or `serve` to render a small proposal set instead of the full site:

```bash
build-eips build --only 555
build-eips serve --only 555
build-eips build --only 555 678
```

CLI `--only` replaces `[render].only` for that run.

Targeted rendering applies only to dirty local `build` and `serve`. It does not apply to `check`, `preview`, `clean`, `changed`, editorial commands, `--clean`, `--staging`, `--production`, or `parity`.

When targeted rendering omits a proposal, links and proposal references to that omitted proposal are rewritten to the public site instead of pointing at missing local output.

During `serve --only`, changes to selected proposals are synced into the mounted build repo. New proposal numbers added while `serve --only` is running require restarting `serve`.

## Serve And Preview

Local serving keeps two distinct modes:

* `build-eips serve` for the runtime dev loop
* `build-eips preview` for serving already-built static output (`build-eips build` must be run first)

`build-eips serve` runs Zola's fast serve mode under the hood. It watches tracked edits in the active proposal repo and incrementally updates the local site for content changes.

It also watches tracked edits under `workspace/theme`. Theme changes can take longer to apply because they affect the whole rendered site. During `serve`, staging a new theme file with `git add` triggers a theme rescan; no extra file edit or restart should be needed.

`serve --clean` ignores active-repo dirty edits but still watches the local theme.

`build-eips preview` serves the resolved output directory for the active repo without invoking Zola, preprocessing markdown, or rebuilding anything. If the output directory does not exist yet, it fails and tells you to run `build-eips build` first.

## Local Server And Base URL

The `[server]` table in `.build-eips.toml` controls the local bind address for both `serve` and `preview`; the default is `127.0.0.1:1111`. Per-command `--host` and `--port` flags override `.build-eips.toml` for one run. These settings do not change build base URLs.

The `[site].base_url` value in `.build-eips.toml` is the default local rendered site URL. Starter `.build-eips.toml` files set it to `http://127.0.0.1:1111`. If you change `[server].port`, update `[site].base_url` too when generated links should match the local server.

Per-command `--base-url` on `build` or `serve` is a one-run override and wins over `.build-eips.toml`, including with staging or parity commands.

`preview` serves existing output, so build with `--base-url` first when previewed HTML should contain a different local link target.

## Remote Environment Commands

Commands that explicitly use remote proposal sources:

```bash
build-eips --staging build
build-eips --production build
build-eips --staging serve
build-eips --production serve
build-eips --staging check
build-eips --production check
```

`parity` is the built-in remote clean staging/parity path:

```bash
build-eips parity build
build-eips parity serve
build-eips parity check
```

Remote environment commands and `parity` ignore `[site].base_url` in `.build-eips.toml`. They still require workspace-local `theme/`; CI and production runners should check out `theme/` at the desired commit or branch before running site commands.

## Source And Output Overrides

Workspace-local sources come from the standard workspace layout. The local theme is `workspace/theme`, and local sibling repos are `workspace/<sibling_repo_id>` from the active repo manifest.

Use `--remote-sibling-repo` when you need to force remote sibling proposal sources for a single command.

Use global `--build-root <path>` when you want a separate prepared repo and output directory, for example to compare two builds side by side. The path replaces the default `.local-build/<repo_id>` location for each command where you pass it, so use the same `--build-root` value when serving or previewing builds.


Example:

```bash
build-eips -C /work/EIPs-project/EIPs --build-root /tmp/eips-local build --base-url http://127.0.0.1:1111
build-eips -C /work/EIPs-project/EIPs --build-root /tmp/eips-staging --staging build --base-url http://127.0.0.1:1112

build-eips -C /work/EIPs-project/EIPs --build-root /tmp/eips-local preview --port 1111
build-eips -C /work/EIPs-project/EIPs --build-root /tmp/eips-staging preview --port 1112

# Or using serve
build-eips -C /work/EIPs-project/EIPs --build-root /tmp/eips-local serve --port 1111
build-eips -C /work/EIPs-project/EIPs --build-root /tmp/eips-staging --staging serve --port 1112
```

## Editorial Commands

Use the explicit editorial command group when you want targeted `eipw` validation:

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

`editorial build` runs targeted editorial validation first, then reuses the runtime `check` path.
