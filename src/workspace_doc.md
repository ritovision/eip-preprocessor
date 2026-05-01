# build-eips Workspace

This directory is a local multi-repo workspace for building, serving, previewing, and validating EIPs/ERCs with the shared theme and proposal sibling repos.

The workspace keeps the proposal repos, theme repo, generated build output, and local workspace settings in one predictable layout. Run commands from an active proposal repo such as `EIPs/` or `ERCs/`, or from this workspace root with `-C EIPs` or `-C ERCs`.

## Workspace Layout

After running a setup script, the minimal operational workspace should look like this:

```text
EIPs-project/
├── .build-eips.toml
├── WORKSPACE.md
├── .local-build/
├── EIPs/
├── ERCs/
└── theme/
```

Optional setup flags can add more repos:

```text
EIPs-project/
├── template/       # --template
├── preprocessor/  # --platform-dev
└── eipw/          # --platform-dev
```

- `.build-eips.toml`: workspace settings.
- `WORKSPACE.md`: generated workspace guide.
- `.local-build/`: generated build output and materialized repositories.
- `EIPs/` and `ERCs/`: proposal source repositories.
- `theme/`: workspace-local Zola theme required by build, serve, and check commands.
- `template/`: optional proposal template repository.
- `preprocessor/`: optional local `build-eips` development checkout.
- `eipw/`: optional local `eipw` development checkout.

If the optional repos are missing, rerun build-eips init with the needed flags.

From an active proposal repo:

```sh
build-eips init .. --template
build-eips init .. --platform-dev
build-eips init .. --template --platform-dev
```

From the workspace root:

```sh
build-eips -C EIPs init . --template
build-eips -C EIPs init . --platform-dev
build-eips -C EIPs init . --template --platform-dev
```

## Requirements And Troubleshooting

Local workspace commands require these tools on `PATH`:

- Git
- `build-eips`
- Zola 0.22.1

Git must be installed separately. The setup scripts locate or install `build-eips` and Zola, add locally installed tool directories to `PATH` for the current shell session, and print guidance for making those `PATH` changes permanent.

Run `build-eips doctor` after setup and whenever a command cannot find a repo, config file, theme, or required tool:

```sh
build-eips doctor
```

From the workspace root, anchor the command through an active proposal repo:

```sh
build-eips -C EIPs doctor
build-eips -C ERCs doctor
```

`build-eips doctor` checks:

- required tools: Git, `build-eips`, and Zola 0.22.1
- the active proposal repo manifest
- `.build-eips.toml`
- workspace-local sibling proposal repos
- workspace-local `theme/`
- optional setup helper tools used by setup scripts

If a fresh shell cannot find `build-eips` or Zola, rerun the setup script or apply the permanent `PATH` guidance printed by the setup script.

If a sibling repo, `theme/`, or optional platform repo is missing, rerun `build-eips init` with the needed flags.

If `build-eips doctor` reports that Zola is missing or too old, rerun the setup script to install the supported Zola version.

## Commands

Run site commands from `EIPs/` or `ERCs/`. From the workspace root, use `-C`:

```sh
build-eips -C EIPs build
build-eips -C ERCs serve
```

Runtime rendering commands require the workspace-local `theme/`.

Common commands:

```sh
build-eips build
build-eips serve
build-eips check
build-eips doctor
```

`build-eips serve` listens at `http://127.0.0.1:1111` by default. Use
`--host`, `--port`, and `--base-url` when a different local address or public
URL is needed.

## Serve And Preview

Local serving keeps two distinct modes:

* `build-eips serve` for the runtime dev loop
* `build-eips preview` for serving already-built static output (`build-eips build` must be run first)

`build-eips serve` runs Zola's fast serve mode under the hood. It watches tracked edits in the active proposal repo and incrementally updates the local site for content changes.

It also watches tracked edits under `theme/` in the workspace. Theme changes can take longer to apply because they affect the whole rendered site. During `serve`, staging a new theme file with `git add` triggers a theme rescan; no extra file edit or restart should be needed.

`serve --clean` ignores active-repo dirty edits but still watches the local theme.

`build-eips preview` serves the resolved output directory for the active repo without invoking Zola, preprocessing markdown, or rebuilding anything. If the output directory does not exist yet, it fails and tells you to run `build-eips build` first.

### Local Server And Base URL

The `[server]` table in `.build-eips.toml` controls the local bind address for both `serve` and `preview`; the default is `127.0.0.1:1111`. Per-command `--host` and `--port` flags override `.build-eips.toml` for one run. These settings do not change build base URLs.

The `[site].base_url` value in `.build-eips.toml` is the default local rendered site URL. Starter `.build-eips.toml` files set it to `http://127.0.0.1:1111`. If you change `[server].port`, update `[site].base_url` too when generated links should match the local server.

Per-command `--base-url` on `build` or `serve` is a one-run override and wins over `.build-eips.toml`, including with staging or parity commands.

`preview` serves existing output, so build with `--base-url` first when previewed HTML should contain a different local link target.

### Target Specific Proposals

Full local `build` and `serve` runs can take time because they process every proposal file. Use targeted rendering when you only need to test a few proposals or theme changes against a small proposal set:

```bash
build-eips serve --only 555
build-eips build --only 555
build-eips build --only 555 678
```

You can also set a default target list in the workspace `.build-eips.toml`:

```toml
[render]
only = [555, 678]
```

CLI `--only` replaces `[render].only` for that command. Clean, staging, production, parity, check, changed, preview, and editorial commands do not use render filtering.

New proposal numbers added while `serve --only` is running require restarting serve.

## Remote Environment Commands

Use remote environment commands when you want a clean render against remote proposal sources instead of the local dirty workspace. These commands are useful for checking staging or production behavior without using tracked working-tree edits from the active proposal repo or local sibling proposal repos.

Commands that explicitly select an environment:

```bash
build-eips --staging build
build-eips --production build
build-eips --staging serve
build-eips --production serve
build-eips --staging check
build-eips --production check
```

`parity` is the built-in remote clean staging/parity path. Use it when you want to compare local tooling behavior against the staging-style remote source setup:

```bash
build-eips parity build
build-eips parity serve
build-eips parity check
```

Remote environment commands and `parity` ignore `[site].base_url` in `.build-eips.toml`; they resolve their environment URLs from the selected mode. Per-command `--base-url` still wins for `build` and `serve`.

These commands still require the workspace-local `theme/`. CI and production runners should check out `theme/` at the desired commit or branch before running site commands.

## Source And Output Overrides

Workspace-local sources come from the standard workspace layout. The local theme is `theme/`, and local sibling proposal repos use their repo IDs, such as `EIPs/` and `ERCs/`.

Use `--remote-siblings` when you need to force remote sibling proposal sources for a single command.

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

## Editorial Validation

Use editorial commands when you want targeted `eipw` validation before opening or updating a pull request.

Both `editorial lint` and `editorial check` take the same selector modes:

* proposal numbers or repo-relative proposal paths for explicit targets
* `--working-tree` for tracked dirty proposal files
* `--against-upstream` for proposal files changed versus the upstream merge-base
* `--batch <path>` for a repeatable target list

They also accept `eipw` options such as `--format github`.

`editorial lint` runs targeted editorial validation:

```bash
build-eips editorial lint 1
build-eips editorial lint --working-tree
build-eips editorial lint --against-upstream --format github
```

`editorial check` runs targeted editorial validation first, then reuses the local `check` path:

```bash
build-eips editorial check 1
build-eips editorial check --working-tree
build-eips editorial check --against-upstream --format github
```

Use a batch file when you want to lint or build-check the same proposal set repeatedly. A batch file is a plain text file with one proposal number or repo-relative proposal path per line:

```txt
1
7949
content/07950.md
```

```bash
build-eips editorial lint --batch ../editor-batch.txt
build-eips editorial check --batch ../editor-batch.txt
```
