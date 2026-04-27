/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Execution source and path resolution.

use std::path::{Path, PathBuf};

use log::{debug, info};
use snafu::{OptionExt, ResultExt, Whatever};
use url::Url;

use crate::{
    cli::{Args, Operation, ServerCliArgs},
    config::{self, Config, LoadedWorkspaceConfig, ServerBinding},
    context::{resolve_input_path, root},
    git,
    identity::ActiveRepoIdentity,
    layout::BUILD_DIR,
    theme::ThemeSource,
};

#[derive(Debug, Clone)]
pub(crate) struct ResolvedExecution {
    pub(crate) root_path: PathBuf,
    pub(crate) build_path: PathBuf,
    pub(crate) repository_use: git::RepositoryUse,
    pub(crate) theme: ThemeSource,
    pub(crate) source_materialization: git::SourceMaterialization,
    pub(crate) server_binding: ServerBinding,
    pub(crate) base_url_override: Option<Url>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectedSource {
    WorkspaceLocal,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutionSettings {
    pub(crate) build_root: Option<PathBuf>,
    pub(crate) staging: bool,
    pub(crate) allow_dirty: bool,
    pub(crate) theme: SelectedSource,
    pub(crate) sibling: SelectedSource,
}

fn has_execution_override_flags(args: &Args) -> bool {
    args.staging
        || args.production
        || args.remote_theme
        || args.remote_sibling_repo
        || args.build_root.is_some()
}

pub(crate) fn validate_non_execution_command_flags(args: &Args) -> Result<(), Whatever> {
    if args.operation.is_workspace_command() && has_execution_override_flags(args) {
        snafu::whatever!("execution override flags cannot be used with `workspace` commands");
    }

    if args.operation.is_print_command() && has_execution_override_flags(args) {
        snafu::whatever!("execution override flags cannot be used with `print`");
    }

    Ok(())
}

fn resolve_bool_override(
    enabled: bool,
    disabled: bool,
    enabled_flag: &str,
    disabled_flag: &str,
) -> Result<Option<bool>, Whatever> {
    match (enabled, disabled) {
        (true, true) => {
            snafu::whatever!("cannot pass both `{enabled_flag}` and `{disabled_flag}`")
        }
        (true, false) => Ok(Some(true)),
        (false, true) => Ok(Some(false)),
        (false, false) => Ok(None),
    }
}

fn remote_source_override(force_remote: bool) -> Option<SelectedSource> {
    force_remote.then_some(SelectedSource::Remote)
}

fn format_sibling_ids(sibling_ids: &[String]) -> String {
    sibling_ids.join(", ")
}

fn resolve_environment_override(args: &Args) -> Result<Option<bool>, Whatever> {
    resolve_bool_override(args.staging, args.production, "--staging", "--production")
}

pub(crate) fn explicit_environment_or_parity(args: &Args) -> Result<Option<bool>, Whatever> {
    if let Some(staging) = resolve_environment_override(args)? {
        return Ok(Some(staging));
    }

    if matches!(args.operation, Operation::Parity { .. }) {
        return Ok(Some(true));
    }

    Ok(None)
}

pub(crate) fn resolve_execution_settings(
    args: &Args,
    sibling_ids: &[String],
    workspace_config: Option<&LoadedWorkspaceConfig>,
) -> Result<ExecutionSettings, Whatever> {
    let build_root = args
        .build_root
        .as_deref()
        .map(resolve_input_path)
        .transpose()?;
    let explicit_environment = explicit_environment_or_parity(args)?;
    let theme_override = remote_source_override(args.remote_theme);
    let sibling_override = remote_source_override(args.remote_sibling_repo);
    let clean = args.operation.clean_cli_args().clean;

    let (staging, allow_dirty, default_theme, default_sibling) = if let Some(staging) =
        explicit_environment
    {
        (
            staging,
            false,
            SelectedSource::Remote,
            SelectedSource::Remote,
        )
    } else if args.operation.is_plain_site_command() || args.operation.is_editorial_build_command()
    {
        (
            true,
            !clean,
            SelectedSource::WorkspaceLocal,
            SelectedSource::WorkspaceLocal,
        )
    } else {
        (false, false, SelectedSource::Remote, SelectedSource::Remote)
    };

    let missing_theme = theme_override.is_none()
        && default_theme == SelectedSource::WorkspaceLocal
        && workspace_config.is_none();
    let missing_sibling = sibling_override.is_none()
        && default_sibling == SelectedSource::WorkspaceLocal
        && !sibling_ids.is_empty()
        && workspace_config.is_none();

    if missing_theme || missing_sibling {
        let required_sources = match (missing_theme, missing_sibling) {
            (true, true) => "theme and sibling",
            (true, false) => "theme",
            (false, true) => "sibling",
            (false, false) => unreachable!(),
        };

        snafu::whatever!(
            "the selected command requires workspace-local {required_sources} sources, but no `{}` was found to provide them.\nResolve this by doing one of the following:\n1. run `build-eips workspace init <workspace-root>` so the workspace config supplies the local sources\n2. pass `--remote-theme` and/or `--remote-sibling-repo` for remote source overrides\n3. use `parity <command>`, `--staging <command>`, or `--production <command>` for remote clean environment behavior",
            config::LOCAL_CONFIG_FILE
        );
    }

    let theme = theme_override.unwrap_or(default_theme);
    let sibling = sibling_override.unwrap_or(default_sibling);

    Ok(ExecutionSettings {
        build_root,
        staging,
        allow_dirty,
        theme,
        sibling,
    })
}

fn local_repo_url(path: &Path) -> Result<Url, Whatever> {
    Url::from_directory_path(path)
        .ok()
        .whatever_context("unable to convert local sibling repository path into a file URL")
}

fn local_repo_available(path: &Path) -> bool {
    git2::Repository::open(path).is_ok()
}

fn apply_sibling_sources(
    repository_use: &mut git::RepositoryUse,
    sibling_ids: &[String],
    workspace_config: Option<&LoadedWorkspaceConfig>,
    sibling: &SelectedSource,
) -> Result<(), Whatever> {
    match sibling {
        SelectedSource::Remote => Ok(()),
        SelectedSource::WorkspaceLocal => {
            if sibling_ids.is_empty() {
                return Ok(());
            }

            let workspace_config = workspace_config.whatever_context(
                "workspace-local sibling selection requires a workspace config",
            )?;
            let mut missing = Vec::new();
            let mut local_repositories = Vec::new();

            for repo_id in sibling_ids {
                let path = workspace_config.local_repo_path(repo_id);
                if local_repo_available(&path) {
                    local_repositories.push((repo_id.clone(), local_repo_url(&path)?));
                } else {
                    missing.push(repo_id.clone());
                }
            }

            if !missing.is_empty() {
                snafu::whatever!(
                    "workspace-local sibling selection requires all declared sibling repos; missing or invalid sibling repo(s): {}",
                    format_sibling_ids(&missing)
                );
            }

            for (repo_id, url) in local_repositories {
                repository_use.other_repos.insert(repo_id, url);
            }

            Ok(())
        }
    }
}

fn build_path(
    root_path: &Path,
    repository_use: &git::RepositoryUse,
    workspace_config: Option<&LoadedWorkspaceConfig>,
    build_root: Option<&Path>,
) -> PathBuf {
    build_root
        .map(Path::to_path_buf)
        .or_else(|| {
            workspace_config.map(|workspace_config| {
                workspace_config.workspace_build_root(&repository_use.title)
            })
        })
        .unwrap_or_else(|| root_path.join(BUILD_DIR))
}

fn theme_source(
    baseline: &Config,
    workspace_config: Option<&LoadedWorkspaceConfig>,
    theme: &SelectedSource,
) -> ThemeSource {
    match theme {
        SelectedSource::WorkspaceLocal => ThemeSource::Local {
            path: workspace_config
                .expect("workspace-local theme selection requires a workspace config")
                .local_theme_path(),
        },
        SelectedSource::Remote => ThemeSource::Remote {
            repository: baseline.theme.repository.to_string(),
            commit: baseline.theme.commit.clone(),
        },
    }
}

pub(crate) fn resolve_server_binding(
    workspace_config: Option<&LoadedWorkspaceConfig>,
    server_cli: &ServerCliArgs,
) -> ServerBinding {
    let mut binding = workspace_config
        .map(|workspace_config| ServerBinding::from(workspace_config.server_settings()))
        .unwrap_or_default();

    if let Some(host) = &server_cli.host {
        binding.host = host.clone();
    }

    if let Some(port) = server_cli.port {
        binding.port = port;
    }

    binding
}

pub(crate) fn resolve_base_url_override(
    args: &Args,
    workspace_config: Option<&LoadedWorkspaceConfig>,
) -> Result<Option<Url>, Whatever> {
    if let Some(base_url) = args.operation.base_url_cli_args().base_url {
        return Ok(Some(base_url));
    }

    if explicit_environment_or_parity(args)?.is_some() {
        return Ok(None);
    }

    Ok(workspace_config.and_then(|config| config.site_settings().base_url.clone()))
}

pub(crate) fn resolve_execution(args: &Args) -> Result<ResolvedExecution, Whatever> {
    let root_path = root(args)?;
    let active_repo = ActiveRepoIdentity::load(&root_path)?;
    let sibling_ids = active_repo.sibling_ids();
    let workspace_config = LoadedWorkspaceConfig::discover(&root_path)
        .whatever_context("unable to load workspace config")?;

    if let Some(workspace_config) = workspace_config.as_ref() {
        debug!(
            "using workspace config `{}`",
            workspace_config.config_path().to_string_lossy()
        );
    }

    let settings = resolve_execution_settings(args, &sibling_ids, workspace_config.as_ref())?;
    let baseline = if settings.staging {
        Config::staging()
    } else {
        Config::production()
    };

    let mut repository_use = active_repo.repository_use(settings.staging)?;
    apply_sibling_sources(
        &mut repository_use,
        &sibling_ids,
        workspace_config.as_ref(),
        &settings.sibling,
    )?;

    let build_path = build_path(
        &root_path,
        &repository_use,
        workspace_config.as_ref(),
        settings.build_root.as_deref(),
    );
    let theme = theme_source(&baseline, workspace_config.as_ref(), &settings.theme);
    let source_materialization = if settings.allow_dirty {
        info!(
            "dirty mode is enabled; tracked working-tree changes from the active content repo will be materialized into the build input"
        );
        git::SourceMaterialization::Dirty
    } else {
        git::SourceMaterialization::Clean
    };
    let base_url_override = resolve_base_url_override(args, workspace_config.as_ref())?;

    Ok(ResolvedExecution {
        root_path,
        build_path,
        repository_use,
        theme,
        source_materialization,
        server_binding: resolve_server_binding(
            workspace_config.as_ref(),
            &args.operation.server_cli_args(),
        ),
        base_url_override,
    })
}
