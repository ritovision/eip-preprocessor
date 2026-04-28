# Preprocessor Module And Test Ownership

This note records module and test ownership for the preprocessor crate. It is a
review rubric for module boundaries, not user-facing command documentation.

## Module Ownership

- `main.rs` owns CLI entry, top-level dispatch, build locking, and runtime
  orchestration.
- `cache.rs` owns local cache directory discovery, locking, and cache path
  creation.
- `cli.rs` owns the clap command surface and command helper methods.
- `config.rs` owns built-in repository metadata, workspace config schema, repo
  manifest schema, parsing, defaults, and config discovery.
- `context.rs` owns command input path resolution and workspace command context.
- `editorial.rs` owns editorial target selection and editorial runtime helpers.
- `execution.rs` owns source mode, environment, base URL, server binding, build
  path, workspace source, and runtime execution resolution.
- `find_root.rs` owns active proposal-repo root detection.
- `git.rs` owns git repository identification, clone/fetch/merge behavior,
  source materialization, and tracked path synchronization.
- `github.rs` owns GitHub annotation reporting support for lint output.
- `identity.rs` owns active repository identity selection from repo manifests or
  legacy metadata.
- `layout.rs` owns shared build layout names and path helpers.
- `lint.rs` owns eipw lint configuration and invocation.
- `markdown.rs` owns proposal markdown preprocessing.
- `pipeline.rs` owns prepared Zola runtime setup and build/check/serve steps.
- `preview.rs` owns static preview serving for already-built output.
- `print.rs` owns diagnostic print subcommands.
- `progress.rs` owns progress/log rendering helpers.
- `proposal.rs` owns proposal path classification and future proposal-number
  helpers.
- `serve.rs` owns dirty active-repo and local-theme serve synchronization.
- `theme.rs` owns shared theme source identity.
- `workspace.rs` owns local workspace initialization and diagnostics.
- `zola.rs` owns Zola discovery, theme mounting, and Zola command invocation.

## Test Ownership

New tests should generally live in the module that owns the behavior. Use
`super::` from module-local tests where natural, or sibling module paths from the
owning module. Module-local tests now cover `cli.rs`, `execution.rs`,
`serve.rs`, `workspace.rs`, `editorial.rs`, `pipeline.rs`, and the existing
`config.rs`, `git.rs`, and `zola.rs` tests; `src/tests.rs` intentionally holds
the remaining cross-domain behavior tests.

Use `src/tests.rs` for cross-domain behavior tests, especially tests covering:

- command dispatch across modules
- CLI plus execution plus runtime behavior
- downstream CI invariance
- multi-repo or manifest-driven flows
- workspace plus execution plus editorial behavior
- source materialization behavior spanning git, execution, pipeline, or serve
- tests that would require exposing more internals just to move them

Move tests to module-local `#[cfg(test)]` modules only when the behavior is
owned by one module and the test remains clearer there. Examples include pure
clap parsing in `cli.rs`, execution policy helpers in `execution.rs`, serve
event filtering in `serve.rs`, direct theme preparation in `pipeline.rs`,
editorial helper behavior in `editorial.rs`, workspace init/doctor behavior in
`workspace.rs`, and active repo identity behavior in `identity.rs` when it does
not require the full execution path. `proposal.rs` owns proposal path
classification and future proposal-number tests; the Phase 0 inventory found no
current proposal-only tests in `src/tests.rs`.

## Dependency Direction

Imports should generally point from higher-level orchestration toward
lower-level or shared modules:

```text
main -> editorial/workspace/serve/pipeline -> context/identity/execution -> cli/layout/theme/proposal
```

Lower-level or shared modules should not import higher-level orchestration
modules such as `workspace`, `pipeline`, or `main`.

## Visibility

Do not widen visibility just to move a test. If moving a test needs broad
visibility, a crate-root re-export, or a path through an unrelated higher-level
module, the test likely belongs in `src/tests.rs`. Visibility tightening should
follow test moves and call-site audits rather than drive them.
