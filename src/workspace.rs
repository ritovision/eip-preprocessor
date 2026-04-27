/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Local workspace initialization and diagnostics.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use log::info;
use snafu::{Report, ResultExt, Whatever};
use url::Url;

use crate::{
    cli::Args,
    config::{self, Config, LoadedRepoManifest, LoadedWorkspaceConfig},
    context::{load_workspace_command_context, resolve_input_path, root},
    git,
    identity::ActiveRepoIdentity,
};

const PROPOSAL_TEMPLATE_URL: &str = "https://github.com/eips-wg/template.git";
const PLATFORM_PREPROCESSOR_URL: &str = "https://github.com/eips-wg/preprocessor.git";
const PLATFORM_EIPW_URL: &str = "https://github.com/ethereum/eipw.git";

#[derive(Debug, Clone, Copy)]
enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Default)]
pub(crate) struct DoctorReport {
    pub(crate) warnings: usize,
    pub(crate) failures: usize,
}

pub(crate) struct WorkspaceInitRepositories<'a> {
    pub(crate) theme: &'a Url,
    pub(crate) template: &'a Url,
    pub(crate) preprocessor: &'a Url,
    pub(crate) eipw: &'a Url,
}

impl fmt::Display for DoctorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        };

        f.write_str(label)
    }
}

impl DoctorReport {
    fn record(&mut self, status: DoctorStatus, message: impl AsRef<str>) {
        match status {
            DoctorStatus::Ok => (),
            DoctorStatus::Warn => self.warnings += 1,
            DoctorStatus::Fail => self.failures += 1,
        }

        println!("[{status}] {}", message.as_ref());
    }
}

fn command_path(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    #[cfg(not(windows))]
    let candidates = [command.to_owned()];

    #[cfg(windows)]
    {
        use std::ffi::OsString;

        let mut candidates = vec![command.to_owned()];
        let command = OsString::from(command);
        let path_exts = std::env::var_os("PATHEXT")
            .unwrap_or_default()
            .to_string_lossy()
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| format!("{}{}", command.to_string_lossy(), ext))
            .collect::<Vec<_>>();
        candidates.extend(path_exts);
        std::env::split_paths(&path).find_map(|entry| {
            candidates
                .iter()
                .map(|candidate| entry.join(candidate))
                .find(|candidate| candidate.is_file())
        })
    }

    #[cfg(not(windows))]
    {
        std::env::split_paths(&path).find_map(|entry| {
            candidates
                .iter()
                .map(|candidate| entry.join(candidate))
                .find(|candidate| candidate.is_file())
        })
    }
}

fn check_workspace_repo(
    report: &mut DoctorReport,
    workspace_root: &Path,
    name: &str,
) -> Option<PathBuf> {
    let path = workspace_root.join(name);
    match git2::Repository::open(&path) {
        Ok(_) => {
            report.record(
                DoctorStatus::Ok,
                format!(
                    "found workspace repo `{}` at `{}`",
                    name,
                    path.to_string_lossy()
                ),
            );
            Some(path)
        }
        Err(_) if !path.exists() => {
            report.record(
                DoctorStatus::Fail,
                format!(
                    "expected workspace repo `{}` at `{}`",
                    name,
                    path.to_string_lossy()
                ),
            );
            None
        }
        Err(_) => {
            report.record(
                DoctorStatus::Fail,
                format!(
                    "expected `{}` to be a git repository at `{}`",
                    name,
                    path.to_string_lossy()
                ),
            );
            None
        }
    }
}

fn check_sibling_manifest_id(
    report: &mut DoctorReport,
    sibling_path: &Path,
    expected_repo_id: &str,
) {
    match LoadedRepoManifest::load(sibling_path) {
        Ok(Some(manifest)) if manifest.manifest().repo_id == expected_repo_id => report.record(
            DoctorStatus::Ok,
            format!("sibling `{expected_repo_id}` manifest repo_id matches workspace key"),
        ),
        Ok(Some(manifest)) => report.record(
            DoctorStatus::Fail,
            format!(
                "sibling `{expected_repo_id}` manifest declares repo_id `{}`",
                manifest.manifest().repo_id
            ),
        ),
        Ok(None) => (),
        Err(error) => report.record(
            DoctorStatus::Fail,
            format!(
                "sibling `{expected_repo_id}` repo manifest could not be loaded: {}",
                Report::from_error(error)
            ),
        ),
    }
}

fn check_tool(report: &mut DoctorReport, command: &str, why: &str) {
    match command_path(command) {
        Some(path) => report.record(
            DoctorStatus::Ok,
            format!(
                "found required tool `{}` at `{}`",
                command,
                path.to_string_lossy()
            ),
        ),
        None => report.record(
            DoctorStatus::Fail,
            format!("missing required tool `{}`: {}", command, why),
        ),
    }
}

fn check_optional_download_tool(report: &mut DoctorReport) {
    let curl = command_path("curl");
    let wget = command_path("wget");

    match (curl, wget) {
        (Some(path), _) => report.record(
            DoctorStatus::Ok,
            format!(
                "found front-door download helper `curl` at `{}`",
                path.to_string_lossy()
            ),
        ),
        (None, Some(path)) => report.record(
            DoctorStatus::Ok,
            format!(
                "found front-door download helper `wget` at `{}`",
                path.to_string_lossy()
            ),
        ),
        (None, None) => report.record(
            DoctorStatus::Warn,
            "missing both `curl` and `wget`; `scripts/dev-setup` will not be able to download a release binary",
        ),
    }
}

pub(crate) fn collect_doctor_report(
    args: &Args,
    check_tools: bool,
) -> Result<DoctorReport, Whatever> {
    let context = load_workspace_command_context(args)?;
    let mut report = DoctorReport::default();
    let (root_path, active_repo) = match root(args) {
        Ok(root_path) => match ActiveRepoIdentity::load(&root_path) {
            Ok(active_repo) => {
                report.record(
                    DoctorStatus::Ok,
                    format!(
                        "identified active repo `{}` from {}",
                        active_repo.repo_id(),
                        active_repo.source_description()
                    ),
                );
                if let Some(manifest) = active_repo.manifest() {
                    report.record(
                        DoctorStatus::Ok,
                        format!(
                            "repo manifest parses at `{}`",
                            manifest.manifest_path().to_string_lossy()
                        ),
                    );
                }
                (Some(root_path), Some(active_repo))
            }
            Err(error) => {
                report.record(
                    DoctorStatus::Fail,
                    format!("active repo identity could not be resolved: {error}"),
                );
                (Some(root_path), None)
            }
        },
        Err(error) => {
            report.record(
                DoctorStatus::Fail,
                format!(
                    "active repo root could not be resolved: {}",
                    Report::from_error(error)
                ),
            );
            (None, None)
        }
    };

    match context.config_path.as_ref() {
        Some(path) => report.record(
            DoctorStatus::Ok,
            format!(
                "found workspace config candidate `{}`",
                path.to_string_lossy()
            ),
        ),
        None => report.record(
            DoctorStatus::Fail,
            format!(
                "could not find `{}` while searching upward from `{}`",
                config::LOCAL_CONFIG_FILE,
                context.search_from.to_string_lossy()
            ),
        ),
    }

    let parsed_config = context
        .config_path
        .as_deref()
        .map(LoadedWorkspaceConfig::from_path)
        .transpose();

    match parsed_config {
        Ok(Some(config)) => {
            report.record(
                DoctorStatus::Ok,
                format!(
                    "workspace config parses at `{}`",
                    config.config_path().to_string_lossy()
                ),
            );

            let workspace_root = config.workspace_root();
            if workspace_root.is_dir() {
                report.record(
                    DoctorStatus::Ok,
                    format!(
                        "workspace root exists at `{}`",
                        workspace_root.to_string_lossy()
                    ),
                );
            } else {
                report.record(
                    DoctorStatus::Fail,
                    format!(
                        "workspace root is missing at `{}`",
                        workspace_root.to_string_lossy()
                    ),
                );
            }

            if let (Some(root_path), Some(active_repo)) = (root_path.as_ref(), active_repo.as_ref())
            {
                let expected_root = workspace_root.join(active_repo.repo_id());
                if root_path == &expected_root {
                    report.record(
                        DoctorStatus::Ok,
                        format!(
                            "active repo `{}` is checked out at `{}`",
                            active_repo.repo_id(),
                            expected_root.to_string_lossy()
                        ),
                    );
                } else {
                    report.record(
                        DoctorStatus::Fail,
                        format!(
                            "active repo `{}` should be checked out at `{}`, found `{}`",
                            active_repo.repo_id(),
                            expected_root.to_string_lossy(),
                            root_path.to_string_lossy()
                        ),
                    );
                }

                check_workspace_repo(&mut report, workspace_root, active_repo.repo_id());
                for sibling_id in active_repo.sibling_ids() {
                    if let Some(sibling_path) =
                        check_workspace_repo(&mut report, workspace_root, &sibling_id)
                    {
                        check_sibling_manifest_id(&mut report, &sibling_path, &sibling_id);
                    }
                }
            } else {
                report.record(
                    DoctorStatus::Warn,
                    "workspace repo layout checks were skipped because active repo identity was unavailable",
                );
            }

            check_workspace_repo(&mut report, workspace_root, config::DEFAULT_THEME_DIR);
        }
        Err(error) => {
            report.record(
                DoctorStatus::Fail,
                format!(
                    "workspace config could not be parsed: {}",
                    Report::from_error(error)
                ),
            );
        }
        Ok(None) => (),
    }

    if check_tools {
        check_tool(
            &mut report,
            "build-eips",
            "workspace bootstrap and build-eips commands expect `build-eips` on PATH",
        );
        check_tool(
            &mut report,
            "git",
            "workspace bootstrap and build-eips commands expect git to be available",
        );
        check_tool(
            &mut report,
            "zola",
            "build, check, and serve commands need a working zola binary",
        );
        check_optional_download_tool(&mut report);

        match command_path("tar") {
            Some(path) => report.record(
                DoctorStatus::Ok,
                format!(
                    "found front-door archive tool `tar` at `{}`",
                    path.to_string_lossy()
                ),
            ),
            None => report.record(
                DoctorStatus::Warn,
                "missing `tar`; `scripts/dev-setup` will not be able to unpack the release binary",
            ),
        }
    }

    Ok(report)
}

pub(crate) fn doctor_workspace(args: &Args) -> Result<(), Whatever> {
    let report = collect_doctor_report(args, true)?;

    if report.failures > 0 {
        snafu::whatever!(
            "workspace doctor found {} failing check(s)",
            report.failures
        );
    }

    Ok(())
}

pub(crate) fn init_workspace(
    args: &Args,
    path: PathBuf,
    include_template: bool,
    platform_dev: bool,
) -> Result<(), Whatever> {
    let theme_repository = Config::staging().theme.repository;
    let template_repository = Url::parse(PROPOSAL_TEMPLATE_URL)
        .whatever_context("invalid proposal template repository URL")?;
    let preprocessor_repository = Url::parse(PLATFORM_PREPROCESSOR_URL)
        .whatever_context("invalid platform preprocessor repository URL")?;
    let eipw_repository =
        Url::parse(PLATFORM_EIPW_URL).whatever_context("invalid platform eipw repository URL")?;
    let repositories = WorkspaceInitRepositories {
        theme: &theme_repository,
        template: &template_repository,
        preprocessor: &preprocessor_repository,
        eipw: &eipw_repository,
    };

    init_workspace_with_repositories(args, path, include_template, platform_dev, &repositories)
}

pub(crate) fn init_workspace_with_repositories(
    args: &Args,
    path: PathBuf,
    include_template: bool,
    platform_dev: bool,
    repositories: &WorkspaceInitRepositories<'_>,
) -> Result<(), Whatever> {
    let root_path = root(args)?;
    let active_repo = ActiveRepoIdentity::load(&root_path)?;
    let workspace_root = resolve_input_path(&path)?;
    std::fs::create_dir_all(&workspace_root)
        .whatever_context("unable to create workspace root directory")?;
    let workspace_root = workspace_root
        .canonicalize()
        .whatever_context("unable to canonicalize workspace root directory")?;

    // Workspace init is a local-dev bootstrap path, so it intentionally uses staging URLs.
    let repository_use = active_repo.repository_use(true)?;

    let expected_root = workspace_root.join(&repository_use.title);
    if root_path != expected_root {
        snafu::whatever!(
            "workspace init expects the active repository at `{}`, found `{}`",
            expected_root.to_string_lossy(),
            root_path.to_string_lossy(),
        );
    }

    for (sibling_id, sibling_url) in repository_use.other_repos {
        git::clone_missing_repo(sibling_url.as_str(), &workspace_root.join(&sibling_id))
            .with_whatever_context(|_| {
                format!("unable to clone workspace sibling repo `{sibling_id}`")
            })?;
    }

    git::clone_missing_repo(
        repositories.theme.as_str(),
        &workspace_root.join(config::DEFAULT_THEME_DIR),
    )
    .whatever_context("unable to clone workspace theme repo")?;

    if include_template {
        git::clone_missing_repo(
            repositories.template.as_str(),
            &workspace_root.join("template"),
        )
        .whatever_context("unable to clone workspace template repo")?;
    }

    if platform_dev {
        git::clone_missing_repo(
            repositories.preprocessor.as_str(),
            &workspace_root.join("preprocessor"),
        )
        .whatever_context("unable to clone workspace preprocessor repo")?;
        git::clone_missing_repo(repositories.eipw.as_str(), &workspace_root.join("eipw"))
            .whatever_context("unable to clone workspace eipw repo")?;
    }

    std::fs::create_dir_all(workspace_root.join(config::DEFAULT_BUILD_ROOT_BASE))
        .whatever_context("unable to create local build root")?;

    let config_path = workspace_root.join(config::LOCAL_CONFIG_FILE);
    if config_path.exists() {
        info!(
            "leaving existing workspace config `{}` in place",
            config_path.to_string_lossy()
        );
    } else {
        std::fs::write(&config_path, config::default_workspace_config_text())
            .whatever_context("unable to write workspace config")?;
    }

    Ok(())
}
