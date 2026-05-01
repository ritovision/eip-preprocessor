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
    cli::EditorialSelectorArgs,
    context::resolve_input_path,
    execution::ResolvedExecution,
    git,
    layout::REPO_DIR,
    lint,
    proposal::{
        classify_editorial_number_selector, is_proposal_path,
        resolve_proposal_number_markdown_path, EditorialNumberSelector,
    },
};

fn repo_relative_path(root_path: &Path, path: &Path) -> Result<PathBuf, Whatever> {
    if path.is_absolute() {
        snafu::whatever!(
            "editorial selectors require repo-relative proposal paths, got `{}`",
            path.to_string_lossy()
        );
    }

    let full_path = root_path.join(path);
    let canonical = full_path.canonicalize().with_whatever_context(|_| {
        format!(
            "unable to resolve editorial target `{}`",
            full_path.to_string_lossy()
        )
    })?;

    let relative = canonical
        .strip_prefix(root_path)
        .with_whatever_context(|_| {
            format!(
                "editorial target `{}` escapes the active repository root",
                path.to_string_lossy()
            )
        })?
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

fn normalize_editorial_selector(root_path: &Path, path: PathBuf) -> Result<PathBuf, Whatever> {
    let Some(selector) = path.as_os_str().to_str() else {
        return Ok(path);
    };

    match classify_editorial_number_selector(selector) {
        EditorialNumberSelector::Number(proposal_number) => {
            resolve_proposal_number_markdown_path(root_path, proposal_number)
        }
        EditorialNumberSelector::InvalidNumberLike(_failure) => {
            snafu::whatever!(
                "editorial number selector `{selector}` is invalid; expected a positive proposal number that fits in u32, without signs or commas"
            );
        }
        EditorialNumberSelector::PathLike => Ok(path),
    }
}

fn normalize_editorial_selectors(
    root_path: &Path,
    paths: Vec<PathBuf>,
) -> Result<Vec<PathBuf>, Whatever> {
    paths
        .into_iter()
        .map(|path| normalize_editorial_selector(root_path, path))
        .collect::<Result<_, _>>()
}

pub(crate) fn editorial_targets(
    selectors: &EditorialSelectorArgs,
    resolved: &ResolvedExecution,
) -> Result<Vec<PathBuf>, Whatever> {
    if selectors.selector_count() != 1 {
        snafu::whatever!(
            "choose exactly one editorial selector: explicit proposal targets, `--batch`, `--working-tree`, or `--against-upstream`"
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
    let targets = if strict {
        normalize_editorial_selectors(&resolved.root_path, raw_targets)?
    } else {
        raw_targets
    };
    validate_editorial_targets(&resolved.root_path, targets, strict)
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

    lint::eipw(resolved.theme_path()?, &resolved.root_path, targets, eipw)
        .whatever_context("editorial lint failed")?;

    Ok(true)
}

pub(crate) fn editorial_runtime_execution(
    mut resolved: ResolvedExecution,
    selectors: &EditorialSelectorArgs,
) -> ResolvedExecution {
    if selectors.working_tree {
        resolved.source_materialization = git::SourceMaterialization::Dirty;
    }
    resolved
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use crate::{
        cli::EditorialSelectorArgs,
        config::{self, ServerBinding},
        execution::ResolvedExecution,
    };

    use super::{editorial_runtime_execution, editorial_targets, validate_editorial_targets};

    fn write_file(root: &Path, relative: impl AsRef<Path>, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn resolved_execution(root_path: PathBuf) -> ResolvedExecution {
        ResolvedExecution {
            root_path,
            build_path: PathBuf::from("/workspace/build/Core"),
            repository_use: crate::git::RepositoryUse {
                title: "Core".to_owned(),
                location: config::RepositoryEndpoint {
                    repository: "https://example.test/Core.git".parse().unwrap(),
                    base_url: "https://example.test/Core/".parse().unwrap(),
                },
                other_repos: Default::default(),
            },
            theme_path: Some(PathBuf::from("/workspace/theme")),
            only: None,
            source_materialization: crate::git::SourceMaterialization::Clean,
            server_binding: ServerBinding::default(),
            base_url_override: None,
        }
    }

    fn explicit_selectors(paths: &[&str]) -> EditorialSelectorArgs {
        EditorialSelectorArgs {
            paths: paths.iter().map(|path| PathBuf::from(*path)).collect(),
            batch: None,
            working_tree: false,
            against_upstream: false,
        }
    }

    #[test]
    fn editorial_working_tree_build_still_forces_dirty_runtime_materialization() {
        let resolved = ResolvedExecution {
            root_path: PathBuf::from("/workspace/Core"),
            build_path: PathBuf::from("/workspace/build/Core"),
            repository_use: crate::git::RepositoryUse {
                title: "Core".to_owned(),
                location: config::RepositoryEndpoint {
                    repository: "https://example.test/Core.git".parse().unwrap(),
                    base_url: "https://example.test/Core/".parse().unwrap(),
                },
                other_repos: Default::default(),
            },
            theme_path: Some(PathBuf::from("/workspace/theme")),
            only: None,
            source_materialization: crate::git::SourceMaterialization::Clean,
            server_binding: ServerBinding::default(),
            base_url_override: None,
        };
        let selectors = EditorialSelectorArgs {
            paths: Vec::new(),
            batch: None,
            working_tree: true,
            against_upstream: false,
        };

        assert_eq!(
            editorial_runtime_execution(resolved, &selectors).source_materialization,
            crate::git::SourceMaterialization::Dirty
        );
    }

    #[test]
    fn editorial_explicit_numeric_selectors_resolve_to_markdown_paths() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "content/0004.md", "");
        let resolved = resolved_execution(temp.path().to_path_buf());

        for selector in ["4", "004", "0004"] {
            assert_eq!(
                editorial_targets(&explicit_selectors(&[selector]), &resolved).unwrap(),
                vec![PathBuf::from("content/0004.md")]
            );
        }
    }

    #[test]
    fn editorial_explicit_numeric_selectors_support_multiple_and_dedupe() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "content/0004.md", "");
        write_file(temp.path(), "content/0005/index.md", "");
        let resolved = resolved_execution(temp.path().to_path_buf());

        assert_eq!(
            editorial_targets(&explicit_selectors(&["4", "0004", "005"]), &resolved).unwrap(),
            vec![
                PathBuf::from("content/0004.md"),
                PathBuf::from("content/0005/index.md"),
            ]
        );
    }

    #[test]
    fn editorial_batch_accepts_numbers_paths_comments_and_empty_lines() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "content/0004.md", "");
        write_file(temp.path(), "content/0005/index.md", "");
        let batch_path = temp.path().join("targets.txt");
        write_file(
            temp.path(),
            "targets.txt",
            "\n# comment\n  \n4\ncontent/0005/index.md\n",
        );
        let resolved = resolved_execution(temp.path().to_path_buf());
        let selectors = EditorialSelectorArgs {
            paths: Vec::new(),
            batch: Some(batch_path),
            working_tree: false,
            against_upstream: false,
        };

        assert_eq!(
            editorial_targets(&selectors, &resolved).unwrap(),
            vec![
                PathBuf::from("content/0004.md"),
                PathBuf::from("content/0005/index.md"),
            ]
        );
    }

    #[test]
    fn editorial_explicit_repo_relative_path_selectors_still_work() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "content/0004.md", "");
        let resolved = resolved_execution(temp.path().to_path_buf());

        assert_eq!(
            editorial_targets(&explicit_selectors(&["content/0004.md"]), &resolved).unwrap(),
            vec![PathBuf::from("content/0004.md")]
        );
    }

    #[test]
    fn editorial_invalid_number_like_selectors_fail_with_editorial_error() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "content/0004.md", "");
        let resolved = resolved_execution(temp.path().to_path_buf());

        for selector in [
            "0",
            "+4",
            "-4",
            "4,5",
            "4,,5",
            ",4",
            "4,",
            "+",
            "-",
            "4294967296",
        ] {
            let error = editorial_targets(&explicit_selectors(&[selector]), &resolved)
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!(
                "editorial number selector `{selector}` is invalid"
            )));
            assert!(error.contains(
                "expected a positive proposal number that fits in u32, without signs or commas"
            ));
        }
    }

    #[test]
    fn editorial_path_like_selectors_continue_through_path_validation() {
        let temp = TempDir::new().unwrap();
        let resolved = resolved_execution(temp.path().to_path_buf());

        for selector in ["foo", "draft.md", "4a", "draft-4.md"] {
            write_file(temp.path(), selector, "");

            let error = editorial_targets(&explicit_selectors(&[selector]), &resolved)
                .unwrap_err()
                .to_string();

            assert!(error.contains("is not a supported proposal path"));
            assert!(!error.contains("editorial number selector"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn editorial_non_utf8_selector_continues_through_path_validation() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let temp = TempDir::new().unwrap();
        let resolved = resolved_execution(temp.path().to_path_buf());
        let selectors = EditorialSelectorArgs {
            paths: vec![PathBuf::from(OsStr::from_bytes(b"\xff"))],
            batch: None,
            working_tree: false,
            against_upstream: false,
        };

        let error = editorial_targets(&selectors, &resolved)
            .unwrap_err()
            .to_string();

        assert!(error.contains("unable to resolve editorial target"));
        assert!(!error.contains("editorial number selector"));
    }

    #[test]
    fn non_strict_editorial_target_validation_does_not_normalize_numeric_paths() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "4", "");
        write_file(temp.path(), "content/0004.md", "");

        assert_eq!(
            validate_editorial_targets(temp.path(), vec![PathBuf::from("4")], false).unwrap(),
            Vec::<PathBuf>::new()
        );
    }
}
