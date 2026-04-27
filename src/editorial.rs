/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Editorial target selection and runtime helpers.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use log::info;
use snafu::{ResultExt, Whatever};

use crate::{
    cache, cli::EditorialSelectorArgs, context::resolve_input_path, execution::ResolvedExecution,
    git, layout::REPO_DIR, lint, proposal::is_proposal_path,
};

fn repo_relative_path(root_path: &Path, path: &Path) -> Result<PathBuf, Whatever> {
    if path.is_absolute() {
        snafu::whatever!(
            "editorial selectors require repo-relative proposal paths, got `{}`",
            path.to_string_lossy()
        );
    }

    let full_path = root_path.join(path);
    let canonical = full_path.canonicalize().whatever_context(format!(
        "unable to resolve editorial target `{}`",
        full_path.to_string_lossy()
    ))?;

    let relative = canonical
        .strip_prefix(root_path)
        .whatever_context(format!(
            "editorial target `{}` escapes the active repository root",
            path.to_string_lossy()
        ))?
        .to_path_buf();

    Ok(relative)
}

fn validate_editorial_targets(
    root_path: &Path,
    paths: Vec<PathBuf>,
    strict: bool,
) -> Result<Vec<PathBuf>, Whatever> {
    let mut unique = BTreeSet::new();
    let mut targets = Vec::new();

    for path in paths {
        if path.is_absolute() {
            snafu::whatever!(
                "editorial selectors require repo-relative proposal paths, got `{}`",
                path.to_string_lossy()
            );
        }

        if !strict && !root_path.join(&path).exists() {
            continue;
        }

        let relative = repo_relative_path(root_path, &path)?;

        if !is_proposal_path(&relative) {
            if strict {
                snafu::whatever!(
                    "editorial target `{}` is not a supported proposal path",
                    relative.to_string_lossy()
                );
            }
            continue;
        }

        if unique.insert(relative.clone()) {
            targets.push(relative);
        }
    }

    if strict && targets.is_empty() {
        snafu::whatever!("editorial selector resolved no proposal files");
    }

    Ok(targets)
}

fn read_editorial_batch(path: &Path) -> Result<Vec<PathBuf>, Whatever> {
    let contents =
        std::fs::read_to_string(path).whatever_context("unable to read editorial batch file")?;
    let mut paths = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        paths.push(PathBuf::from(line));
    }

    Ok(paths)
}

pub(crate) fn editorial_targets(
    selectors: &EditorialSelectorArgs,
    resolved: &ResolvedExecution,
) -> Result<Vec<PathBuf>, Whatever> {
    if selectors.selector_count() != 1 {
        snafu::whatever!(
            "choose exactly one editorial selector: explicit proposal paths, `--batch`, `--working-tree`, or `--against-upstream`"
        );
    }

    let raw_targets = if !selectors.paths.is_empty() {
        selectors.paths.clone()
    } else if let Some(batch) = selectors.batch.as_deref() {
        let batch = resolve_input_path(batch)?;
        read_editorial_batch(&batch)?
    } else if selectors.working_tree {
        git::working_tree_paths(&resolved.root_path)
            .whatever_context("unable to resolve working-tree editorial targets")?
    } else {
        let repo_path = resolved.build_path.join(REPO_DIR);
        git::Fresh::new(
            &resolved.root_path,
            &repo_path,
            resolved.repository_use.clone(),
            resolved.source_materialization,
        )
        .whatever_context("initializing build repo for editorial target selection")?
        .clone_src()
        .whatever_context("cloning source repo for editorial target selection")?
        .fetch_upstream()
        .whatever_context("fetching upstream repo for editorial target selection")?
        .changed_files()
        .whatever_context("unable to list editorial targets against upstream")?
    };

    let strict = !selectors.paths.is_empty() || selectors.batch.is_some();
    validate_editorial_targets(&resolved.root_path, raw_targets, strict)
}

pub(crate) fn run_editorial_lint(
    resolved: &ResolvedExecution,
    selectors: &EditorialSelectorArgs,
    eipw: lint::CmdArgs,
) -> Result<bool, Whatever> {
    let targets = editorial_targets(selectors, resolved)?;
    if targets.is_empty() {
        info!("editorial selector resolved no proposal files; skipping editorial lint");
        return Ok(false);
    }

    let cache = cache::Cache::open().whatever_context("unable to open cache")?;

    lint::eipw(&resolved.theme, &cache, &resolved.root_path, targets, eipw)
        .whatever_context("editorial lint failed")?;

    Ok(true)
}

pub(crate) fn editorial_runtime_execution(
    resolved: &ResolvedExecution,
    selectors: &EditorialSelectorArgs,
) -> ResolvedExecution {
    let mut runtime = resolved.clone();
    if selectors.working_tree {
        runtime.source_materialization = git::SourceMaterialization::Dirty;
    }
    runtime
}
