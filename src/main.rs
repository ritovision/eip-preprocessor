/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

mod cache;
mod config;
mod find_root;
mod git;
mod github;
mod lint;
mod markdown;
mod preview;
mod print;
mod progress;
mod zola;

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use clap::{Parser, Subcommand};
use fslock::LockFile;
use log::{debug, info, warn};
use notify::{Event, RecursiveMode, Watcher};
use snafu::{OptionExt, Report, ResultExt, Whatever};
use url::Url;

use crate::config::{Config, LoadedWorkspaceConfig, SelectedProfile, SourceSelection};

const CONTENT_DIR: &str = "content";
const BUILD_DIR: &str = "build";
const REPO_DIR: &str = "repo";
const OUTPUT_DIR: &str = "output";
const PLATFORM_PREPROCESSOR_URL: &str = "https://github.com/eips-wg/preprocessor.git";
const PLATFORM_EIPW_URL: &str = "https://github.com/ethereum/eipw.git";

#[derive(Debug, Clone)]
enum ThemeSource {
    Remote { repository: String, commit: String },
    Local { path: PathBuf },
}

/// Build script for Ethereum EIPs and ERCs.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Use ROOT as the base directory (instead of finding it automatically)
    #[clap(short = 'C')]
    root: Option<PathBuf>,

    /// Load workspace defaults from CONFIG instead of auto-discovering `.build-eips.toml`
    #[clap(long)]
    config: Option<PathBuf>,

    /// Use the named custom or built-in profile
    #[clap(long)]
    profile: Option<String>,

    /// Force the staging repositories and base URLs
    #[clap(long)]
    staging: bool,

    /// Force the production repositories and base URLs
    #[clap(long)]
    no_staging: bool,

    /// Use a local theme checkout at PATH
    #[clap(long)]
    theme: Option<PathBuf>,

    /// Use the configured remote theme instead of a workspace-local theme
    #[clap(long)]
    remote_theme: bool,

    /// Use a local sibling content repository checkout at PATH
    #[clap(long)]
    sibling_repo: Option<PathBuf>,

    /// Use the configured remote sibling content repository
    #[clap(long)]
    remote_sibling_repo: bool,

    /// Write build artifacts under BUILD_ROOT instead of the default location
    #[clap(long)]
    build_root: Option<PathBuf>,

    /// Force dirty mode on for tracked working-tree changes
    #[clap(long)]
    allow_dirty: bool,

    /// Force dirty mode off
    #[clap(long)]
    no_allow_dirty: bool,

    #[clap(subcommand)]
    operation: Operation,
}

#[derive(Debug, Clone, Subcommand)]
enum Operation {
    /// Print various useful things, like available lints
    Print {
        #[command(flatten)]
        print: print::CmdArgs,
    },

    /// Build the project and output HTML
    Build,

    /// Build the project and launch a web server to preview it
    Serve,

    /// Serve the existing built output without rebuilding it
    Preview,

    /// Remove temporary and output files
    Clean,

    /// Analyze the repository and report errors, but don't build HTML files
    Check,

    /// List files changed since the last commit common to both the local and upstream repositories
    Changed {
        /// List all changed files, not just proposals
        #[arg(long, short)]
        all: bool,
        #[clap(long, value_enum, default_value_t)]
        format: ChangedFormat,
    },

    /// Run targeted editorial validation with eipw
    Editorial {
        #[command(subcommand)]
        command: EditorialCommand,
    },

    /// Manage local multi-repo workspace state
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },

    /// Run a normal command with the built-in parity profile
    Parity {
        #[command(subcommand)]
        command: ProfiledOperation,
    },

    /// Run a normal command with the built-in dirty profile
    Dirty {
        #[command(subcommand)]
        command: ProfiledOperation,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum ProfiledOperation {
    /// Build the project and output HTML
    Build,

    /// Build the project and launch a web server to preview it
    Serve,

    /// Serve the existing built output without rebuilding it
    Preview,

    /// Remove temporary and output files
    Clean,

    /// Analyze the repository and report errors, but don't build HTML files
    Check,

    /// List files changed since the last commit common to both the local and upstream repositories
    Changed {
        /// List all changed files, not just proposals
        #[arg(long, short)]
        all: bool,
        #[clap(long, value_enum, default_value_t)]
        format: ChangedFormat,
    },
}

#[derive(Debug, Subcommand, Clone)]
enum WorkspaceCommand {
    /// Create the local workspace config and clone any missing sibling repositories
    Init {
        /// Workspace root directory
        path: PathBuf,

        /// Also clone preprocessor and eipw for platform development
        #[arg(long)]
        platform_dev: bool,
    },

    /// Check whether the local workspace bootstrap is ready for daily commands
    Doctor,
}

#[derive(Debug, Subcommand, Clone)]
enum EditorialCommand {
    /// Run eipw on explicitly selected proposal targets
    Lint {
        #[command(flatten)]
        selectors: EditorialSelectorArgs,

        #[command(flatten)]
        eipw: lint::CmdArgs,
    },

    /// Run targeted editorial validation, then the runtime check path
    Build {
        #[command(flatten)]
        selectors: EditorialSelectorArgs,

        #[command(flatten)]
        eipw: lint::CmdArgs,
    },
}

#[derive(Debug, clap::Args, Clone)]
struct EditorialSelectorArgs {
    /// Repo-relative proposal path(s), such as `content/07949.md`
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Read repo-relative proposal paths from BATCH, one per line
    #[arg(long)]
    batch: Option<PathBuf>,

    /// Select tracked dirty proposal files from the active content repo
    #[arg(long)]
    working_tree: bool,

    /// Select proposal files changed versus the upstream merge-base
    #[arg(long)]
    against_upstream: bool,
}

#[derive(Debug, clap::ValueEnum, Clone, Default)]
enum ChangedFormat {
    #[default]
    Newline,
    Nul,
    Json,
}

#[derive(Debug, Clone)]
struct ResolvedExecution {
    root_path: PathBuf,
    build_path: PathBuf,
    repository_use: git::RepositoryUse,
    theme: ThemeSource,
    source_materialization: git::SourceMaterialization,
}

#[derive(Debug, Clone)]
enum RuntimeOperation {
    Build,
    Serve,
    Preview,
    Clean,
    Check,
    Changed { all: bool, format: ChangedFormat },
    Editorial { command: EditorialCommand },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectedSource {
    WorkspaceLocal,
    ExplicitLocal(PathBuf),
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecutionSettings {
    build_root: Option<PathBuf>,
    staging: bool,
    allow_dirty: bool,
    theme: SelectedSource,
    sibling: SelectedSource,
}

#[derive(Debug, Clone, Copy)]
enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Default)]
struct DoctorReport {
    warnings: usize,
    failures: usize,
}

#[derive(Debug, Clone)]
struct WorkspaceCommandContext {
    search_from: PathBuf,
    config_path: Option<PathBuf>,
}

impl Operation {
    fn profile_alias_name(&self) -> Option<&'static str> {
        match self {
            Self::Parity { .. } => Some(config::PARITY_PROFILE),
            Self::Dirty { .. } => Some(config::DIRTY_PROFILE),
            Self::Print { .. }
            | Self::Build
            | Self::Serve
            | Self::Preview
            | Self::Clean
            | Self::Check
            | Self::Changed { .. }
            | Self::Editorial { .. }
            | Self::Workspace { .. } => None,
        }
    }

    fn runtime_operation(&self) -> Option<RuntimeOperation> {
        match self {
            Self::Print { .. } | Self::Workspace { .. } => None,
            Self::Build => Some(RuntimeOperation::Build),
            Self::Serve => Some(RuntimeOperation::Serve),
            Self::Preview => Some(RuntimeOperation::Preview),
            Self::Clean => Some(RuntimeOperation::Clean),
            Self::Check => Some(RuntimeOperation::Check),
            Self::Changed { all, format } => Some(RuntimeOperation::Changed {
                all: *all,
                format: format.clone(),
            }),
            Self::Editorial { command } => Some(RuntimeOperation::Editorial {
                command: command.clone(),
            }),
            Self::Parity { command } | Self::Dirty { command } => Some(command.runtime_operation()),
        }
    }

    fn is_workspace_command(&self) -> bool {
        matches!(self, Self::Workspace { .. })
    }

    fn is_print_command(&self) -> bool {
        matches!(self, Self::Print { .. })
    }
}

impl ProfiledOperation {
    fn runtime_operation(&self) -> RuntimeOperation {
        match self {
            Self::Build => RuntimeOperation::Build,
            Self::Serve => RuntimeOperation::Serve,
            Self::Preview => RuntimeOperation::Preview,
            Self::Clean => RuntimeOperation::Clean,
            Self::Check => RuntimeOperation::Check,
            Self::Changed { all, format } => RuntimeOperation::Changed {
                all: *all,
                format: format.clone(),
            },
        }
    }
}

impl ChangedFormat {
    fn print_sep(files: &[&Path], sep: &str) {
        let files: Vec<_> = files
            .iter()
            .map(|f| f.to_str().expect("path not UTF-8"))
            .collect();
        if files.iter().any(|f| f.contains(sep)) {
            panic!("changed file path contains separator");
        }
        println!("{}", files.join(sep));
    }

    fn print_json(files: &[&Path]) {
        let stdout = std::io::stdout();
        serde_json::to_writer_pretty(stdout, files).unwrap();
    }

    fn print(&self, files: &[PathBuf], repo_path: &Path) {
        let files: Vec<_> = files
            .iter()
            .map(|f| match f.strip_prefix(repo_path) {
                Ok(p) => p,
                _ => f,
            })
            .collect();

        match self {
            Self::Newline => Self::print_sep(&files, "\n"),
            Self::Nul => Self::print_sep(&files, "\0"),
            Self::Json => Self::print_json(&files),
        }
    }
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

impl EditorialSelectorArgs {
    fn selector_count(&self) -> usize {
        usize::from(!self.paths.is_empty())
            + usize::from(self.batch.is_some())
            + usize::from(self.working_tree)
            + usize::from(self.against_upstream)
    }
}

fn lock(build_path: &Path) -> Result<LockFile, Whatever> {
    let lock_path = build_path.join(".lock");
    let mut lock_file =
        fslock::LockFile::open(&lock_path).whatever_context("unable to open lock file")?;
    let locked = lock_file
        .try_lock_with_pid()
        .whatever_context("unable to lock build directory")?;
    if !locked {
        info!("waiting on build directory...");
        lock_file
            .lock_with_pid()
            .whatever_context("unable to lock build directory")?;
    }
    Ok(lock_file)
}

fn resolve_input_path(path: &Path) -> Result<PathBuf, Whatever> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        let cwd = std::env::current_dir().whatever_context("unable to get current directory")?;
        Ok(cwd.join(path))
    }
}

fn root(args: &Args) -> Result<PathBuf, Whatever> {
    let dir = match &args.root {
        None => find_root::find_root().whatever_context("cannot find repository root")?,
        Some(path) => {
            find_root::is_root(path).whatever_context("invalid root directory")?;
            path.canonicalize()
                .whatever_context("unable to canonicalize root directory")?
        }
    };
    find_root::is_root(&dir).whatever_context("invalid root directory")?;
    Ok(dir)
}

fn workspace_search_start(args: &Args) -> Result<PathBuf, Whatever> {
    match &args.root {
        Some(path) => {
            let path = resolve_input_path(path)?;
            path.canonicalize()
                .whatever_context("unable to canonicalize workspace search path")
        }
        None => std::env::current_dir().whatever_context("unable to get current directory"),
    }
}

fn load_workspace_command_context(args: &Args) -> Result<WorkspaceCommandContext, Whatever> {
    let search_from = workspace_search_start(args)?;
    let config_path = match args.config.as_deref() {
        Some(path) => Some(resolve_input_path(path)?),
        None => config::discover_path(&search_from),
    };

    Ok(WorkspaceCommandContext {
        search_from,
        config_path,
    })
}

fn has_execution_override_flags(args: &Args) -> bool {
    args.profile.is_some()
        || args.staging
        || args.no_staging
        || args.theme.is_some()
        || args.remote_theme
        || args.sibling_repo.is_some()
        || args.remote_sibling_repo
        || args.build_root.is_some()
        || args.allow_dirty
        || args.no_allow_dirty
}

fn validate_non_execution_command_flags(args: &Args) -> Result<(), Whatever> {
    if args.operation.is_workspace_command() {
        if args.profile.is_some() {
            snafu::whatever!("`--profile` cannot be used with `workspace` commands");
        }

        if has_execution_override_flags(args) {
            snafu::whatever!("execution override flags cannot be used with `workspace` commands");
        }
    }

    if args.operation.is_print_command() && has_execution_override_flags(args) {
        snafu::whatever!("execution override flags cannot be used with `print`");
    }

    Ok(())
}

fn requested_profile_name(args: &Args) -> Result<Option<&str>, Whatever> {
    match (args.operation.profile_alias_name(), args.profile.as_deref()) {
        (Some(alias), Some(profile)) => {
            snafu::whatever!("cannot combine profile alias `{alias}` with `--profile {profile}`")
        }
        (Some(alias), None) => Ok(Some(alias)),
        (None, profile) => Ok(profile),
    }
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

fn resolve_source_override(
    local_path: Option<PathBuf>,
    force_remote: bool,
    local_flag: &str,
    remote_flag: &str,
) -> Result<Option<SelectedSource>, Whatever> {
    match (local_path, force_remote) {
        (Some(_), true) => snafu::whatever!("cannot pass both `{local_flag}` and `{remote_flag}`"),
        (Some(path), false) => Ok(Some(SelectedSource::ExplicitLocal(path))),
        (None, true) => Ok(Some(SelectedSource::Remote)),
        (None, false) => Ok(None),
    }
}

fn resolve_execution_settings(
    args: &Args,
    workspace_config: Option<&LoadedWorkspaceConfig>,
    selected_profile: Option<&SelectedProfile>,
) -> Result<ExecutionSettings, Whatever> {
    let build_root = args
        .build_root
        .as_deref()
        .map(resolve_input_path)
        .transpose()?;
    let staging =
        resolve_bool_override(args.staging, args.no_staging, "--staging", "--no-staging")?
            .unwrap_or_else(|| {
                selected_profile
                    .map(|profile| profile.profile.staging)
                    .unwrap_or(false)
            });
    let allow_dirty = resolve_bool_override(
        args.allow_dirty,
        args.no_allow_dirty,
        "--allow-dirty",
        "--no-allow-dirty",
    )?
    .unwrap_or_else(|| {
        selected_profile
            .map(|profile| profile.profile.allow_dirty)
            .unwrap_or(false)
    });
    let theme_override = resolve_source_override(
        args.theme.as_deref().map(resolve_input_path).transpose()?,
        args.remote_theme,
        "--theme",
        "--remote-theme",
    )?;
    let sibling_override = resolve_source_override(
        args.sibling_repo
            .as_deref()
            .map(resolve_input_path)
            .transpose()?,
        args.remote_sibling_repo,
        "--sibling-repo",
        "--remote-sibling-repo",
    )?;

    let default_theme = selected_profile
        .map(|profile| profile.profile.theme)
        .unwrap_or(SourceSelection::Remote);
    let default_sibling = selected_profile
        .map(|profile| profile.profile.sibling)
        .unwrap_or(SourceSelection::Remote);
    let missing_theme = theme_override.is_none()
        && default_theme == SourceSelection::Local
        && workspace_config.is_none();
    let missing_sibling = sibling_override.is_none()
        && default_sibling == SourceSelection::Local
        && workspace_config.is_none();

    if missing_theme || missing_sibling {
        let profile_name = selected_profile
            .map(|profile| profile.name.as_str())
            .unwrap_or("selected profile");
        let required_sources = match (missing_theme, missing_sibling) {
            (true, true) => "theme and sibling",
            (true, false) => "theme",
            (false, true) => "sibling",
            (false, false) => unreachable!(),
        };

        snafu::whatever!(
            "profile `{profile_name}` requires workspace-local {required_sources} sources, but no `{}` was found to provide them.\nResolve this by doing one of the following:\n1. run `build-eips workspace init <workspace-root>` so the workspace config supplies the local sources\n2. pass `--theme <path>` and/or `--sibling-repo <path>` for local overrides\n3. pass `--remote-theme` and/or `--remote-sibling-repo` for remote overrides\n4. switch to `--profile parity` if remote defaults are what you actually want",
            config::LOCAL_CONFIG_FILE
        );
    }

    let theme = theme_override.unwrap_or(match default_theme {
        SourceSelection::Local => SelectedSource::WorkspaceLocal,
        SourceSelection::Remote => SelectedSource::Remote,
    });
    let sibling = sibling_override.unwrap_or(match default_sibling {
        SourceSelection::Local => SelectedSource::WorkspaceLocal,
        SourceSelection::Remote => SelectedSource::Remote,
    });

    Ok(ExecutionSettings {
        build_root,
        staging,
        allow_dirty,
        theme,
        sibling,
    })
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

fn check_workspace_repo(report: &mut DoctorReport, workspace_root: &Path, name: &str) {
    let path = workspace_root.join(name);
    if !path.exists() {
        report.record(
            DoctorStatus::Fail,
            format!(
                "expected workspace repo `{}` at `{}`",
                name,
                path.to_string_lossy()
            ),
        );
        return;
    }

    match git2::Repository::open(&path) {
        Ok(_) => report.record(
            DoctorStatus::Ok,
            format!(
                "found workspace repo `{}` at `{}`",
                name,
                path.to_string_lossy()
            ),
        ),
        Err(_) => report.record(
            DoctorStatus::Fail,
            format!(
                "expected `{}` to be a git repository at `{}`",
                name,
                path.to_string_lossy()
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

fn doctor_workspace(args: &Args) -> Result<(), Whatever> {
    let context = load_workspace_command_context(args)?;
    let mut report = DoctorReport::default();

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

    if let Ok(Some(config)) = parsed_config.as_ref() {
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

        for repo_name in ["EIPs", "ERCs", config::DEFAULT_THEME_DIR] {
            check_workspace_repo(&mut report, workspace_root, repo_name);
        }
    } else if let Err(error) = parsed_config {
        report.record(
            DoctorStatus::Fail,
            format!(
                "workspace config could not be parsed: {}",
                Report::from_error(error)
            ),
        );
        report.record(
            DoctorStatus::Warn,
            "workspace layout checks were skipped because the workspace config could not be parsed",
        );
    } else if context.config_path.is_some() {
        report.record(
            DoctorStatus::Fail,
            "workspace config could not be parsed, so workspace layout checks were skipped",
        );
    } else {
        report.record(
            DoctorStatus::Warn,
            "workspace layout checks were skipped because no workspace config was found",
        );
    }

    check_tool(
        &mut report,
        "build-eips",
        "workspace bootstrap and daily commands expect `build-eips` on PATH",
    );
    check_tool(
        &mut report,
        "git",
        "workspace bootstrap and daily commands expect git to be available",
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

    if report.failures > 0 {
        snafu::whatever!(
            "workspace doctor found {} failing check(s)",
            report.failures
        );
    }

    Ok(())
}

fn make_build_dir(build_path: &Path) -> Result<PathBuf, Whatever> {
    if let Err(e) = std::fs::create_dir_all(build_path) {
        debug!(
            "got while creating build directory: {}",
            Report::from_error(e)
        );
    }
    Ok(build_path.to_path_buf())
}

fn apply_local_sibling_repo(
    repository_use: &mut git::RepositoryUse,
    path: &Path,
) -> Result<(), Whatever> {
    repository_use
        .only_other_repo()
        .whatever_context("local sibling overrides require exactly one sibling repository")?;

    let url = Url::from_directory_path(path)
        .ok()
        .whatever_context("unable to convert local sibling repository path into a file URL")?;

    for repository in repository_use.other_repos.values_mut() {
        *repository = url.clone();
    }

    Ok(())
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
            workspace_config
                .map(|workspace_config| workspace_config.build_root_for(&repository_use.title))
        })
        .unwrap_or_else(|| root_path.join(BUILD_DIR))
}

fn output_path(build_path: &Path) -> PathBuf {
    build_path.join(OUTPUT_DIR)
}

fn theme_source(
    baseline: &Config,
    workspace_config: Option<&LoadedWorkspaceConfig>,
    theme: &SelectedSource,
) -> ThemeSource {
    match theme {
        SelectedSource::ExplicitLocal(path) => ThemeSource::Local { path: path.clone() },
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

fn sibling_repo_path(
    repository_use: &git::RepositoryUse,
    workspace_config: Option<&LoadedWorkspaceConfig>,
    sibling: &SelectedSource,
) -> Result<Option<PathBuf>, Whatever> {
    match sibling {
        SelectedSource::ExplicitLocal(path) => Ok(Some(path.clone())),
        SelectedSource::WorkspaceLocal => {
            let workspace_config = workspace_config.whatever_context(
                "workspace-local sibling selection requires a workspace config",
            )?;
            let (other_name, _) = repository_use.only_other_repo().whatever_context(
                "local sibling overrides require exactly one sibling repository",
            )?;

            Ok(Some(workspace_config.local_repo_path(other_name)))
        }
        SelectedSource::Remote => Ok(None),
    }
}

fn resolve_execution(args: &Args) -> Result<ResolvedExecution, Whatever> {
    let root_path = root(args)?;
    let workspace_config = LoadedWorkspaceConfig::load(args.config.as_deref(), &root_path)
        .whatever_context("unable to load workspace config")?;
    let requested_profile = requested_profile_name(args)?;
    let selected_profile = match workspace_config.as_ref() {
        Some(workspace_config) => workspace_config
            .selected_profile(requested_profile)
            .whatever_context("unable to select profile")?,
        None => config::selected_profile(None, requested_profile)
            .whatever_context("unable to select profile")?,
    };

    if let Some(workspace_config) = workspace_config.as_ref() {
        debug!(
            "using workspace config `{}`",
            workspace_config.config_path().to_string_lossy()
        );
    }

    if let Some(profile) = selected_profile.as_ref() {
        info!("using selected profile `{}`", profile.name);
    }

    let settings =
        resolve_execution_settings(args, workspace_config.as_ref(), selected_profile.as_ref())?;
    let baseline = if settings.staging {
        Config::staging()
    } else {
        Config::production()
    };

    let mut repository_use = baseline
        .locations
        .identify_repository(&root_path)
        .whatever_context("cannot identify repository use")?;

    let sibling_repo_path = sibling_repo_path(
        &repository_use,
        workspace_config.as_ref(),
        &settings.sibling,
    )?;

    if let Some(path) = sibling_repo_path {
        apply_local_sibling_repo(&mut repository_use, &path)?;
    }

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

    Ok(ResolvedExecution {
        root_path,
        build_path,
        repository_use,
        theme,
        source_materialization,
    })
}

fn is_proposal_path(path: &Path) -> bool {
    let mut path = path.to_path_buf();

    match path.file_name() {
        Some(name) if name == "index.md" => {
            path.pop();
        }
        Some(_)
            if path
                .extension()
                .map(|extension| extension == "md")
                .unwrap_or(false) =>
        {
            path.set_extension("");
        }
        None | Some(_) => return false,
    }

    match path.file_name().and_then(OsStr::to_str) {
        None => return false,
        Some(name) if name.parse::<u64>().is_err() => return false,
        Some(_) => {
            path.pop();
        }
    }

    match path.file_name() {
        Some(name) if name == CONTENT_DIR => {
            path.pop();
        }
        _ => return false,
    }

    path == OsStr::new("")
}

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

fn editorial_targets(
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

#[derive(Debug)]
struct DirtyServeWatcher {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl DirtyServeWatcher {
    fn start(source_root: PathBuf, build_repo_path: PathBuf) -> Result<Self, Whatever> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            dirty_serve_sync_loop(source_root, build_repo_path, stop_thread, ready_tx)
        });

        match ready_rx
            .recv()
            .whatever_context("dirty serve watcher exited before initialization")?
        {
            Ok(()) => Ok(Self { stop, thread }),
            Err(message) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                snafu::whatever!("{message}");
            }
        }
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

fn path_is_watched_source_path(root_path: &Path, path: &Path) -> bool {
    let Ok(relative_path) = path.strip_prefix(root_path) else {
        return false;
    };

    relative_path
        .components()
        .next()
        .map(|component| component.as_os_str() != OsStr::new(".git"))
        .unwrap_or(false)
}

fn event_has_watched_source_path(root_path: &Path, event: &Event) -> bool {
    event
        .paths
        .iter()
        .any(|path| path_is_watched_source_path(root_path, path))
}

fn sync_dirty_serve_state(
    source_root: &Path,
    build_repo_path: &Path,
    previous_dirty_paths: &mut BTreeSet<PathBuf>,
) -> Result<(), Whatever> {
    let current_dirty_paths: BTreeSet<_> = git::working_tree_paths(source_root)
        .whatever_context("unable to list tracked dirty paths for dirty serve")?
        .into_iter()
        .collect();

    let affected_paths: BTreeSet<_> = previous_dirty_paths
        .union(&current_dirty_paths)
        .cloned()
        .collect();

    if affected_paths.is_empty() {
        *previous_dirty_paths = current_dirty_paths;
        return Ok(());
    }

    git::sync_materialized_paths(source_root, build_repo_path, &affected_paths)
        .whatever_context("unable to synchronize tracked paths into the materialized repo")?;
    markdown::preprocess_paths(&build_repo_path.join(CONTENT_DIR), &affected_paths)
        .whatever_context("unable to preprocess synchronized markdown during dirty serve")?;

    info!(
        "synchronized {} tracked path(s) into the materialized repo for dirty serve",
        affected_paths.len()
    );

    *previous_dirty_paths = current_dirty_paths;
    Ok(())
}

fn dirty_serve_sync_loop(
    source_root: PathBuf,
    build_repo_path: PathBuf,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<(), String>>,
) {
    let (event_tx, event_rx) = mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |result| {
        let _ = event_tx.send(result);
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("unable to start dirty serve watcher: {error}")));
            return;
        }
    };

    if let Err(error) = watcher.watch(&source_root, RecursiveMode::Recursive) {
        let _ = ready_tx.send(Err(format!(
            "unable to watch `{}` for dirty serve changes: {error}",
            source_root.to_string_lossy()
        )));
        return;
    }

    let mut previous_dirty_paths: BTreeSet<_> = match git::working_tree_paths(&source_root) {
        Ok(paths) => paths.into_iter().collect(),
        Err(error) => {
            let _ = ready_tx.send(Err(format!(
                "unable to capture initial dirty serve state: {}",
                Report::from_error(error)
            )));
            return;
        }
    };

    info!(
        "watching `{}` for dirty serve changes",
        source_root.to_string_lossy()
    );
    let _ = ready_tx.send(Ok(()));

    while !stop.load(Ordering::Relaxed) {
        let first_event = match event_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let Some(first_event) = first_event else {
            continue;
        };

        let mut saw_relevant_event = match first_event {
            Ok(event) => event_has_watched_source_path(&source_root, &event),
            Err(error) => {
                warn!("filesystem watcher error: {error}");
                false
            }
        };

        loop {
            match event_rx.recv_timeout(Duration::from_millis(75)) {
                Ok(Ok(event)) => {
                    saw_relevant_event |= event_has_watched_source_path(&source_root, &event);
                }
                Ok(Err(error)) => warn!("filesystem watcher error: {error}"),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        if !saw_relevant_event {
            continue;
        }

        if let Err(error) =
            sync_dirty_serve_state(&source_root, &build_repo_path, &mut previous_dirty_paths)
        {
            warn!(
                "unable to synchronize dirty serve changes: {}",
                Report::from_error(error)
            );
        }
    }
}

#[derive(Debug)]
struct Prepared {
    cache: cache::Cache,
    repo_path: PathBuf,
    output_path: PathBuf,
    repository_use: git::RepositoryUse,
    theme: ThemeSource,
    source_root: PathBuf,
    source_materialization: git::SourceMaterialization,
}

impl Prepared {
    fn prepare(resolved: ResolvedExecution) -> Result<Self, Whatever> {
        zola::find_zola().whatever_context("unable to find suitable zola binary")?;

        let ResolvedExecution {
            root_path,
            build_path,
            repository_use,
            theme,
            source_materialization,
        } = resolved;

        let repo_path = build_path.join(REPO_DIR);
        let content_path = repo_path.join(CONTENT_DIR);
        let output_path = output_path(&build_path);

        let both = git::Fresh::new(
            &root_path,
            &repo_path,
            repository_use.clone(),
            source_materialization,
        )
        .whatever_context("initializing build repo")?
        .clone_src()
        .whatever_context("cloning source repo")?
        .fetch_upstream()
        .whatever_context("fetching upstream repo")?;

        both.merge()
            .whatever_context("unable to merge ERC/EIP repositories")?;

        let cache = cache::Cache::open().whatever_context("unable to open cache")?;

        markdown::preprocess(&content_path).whatever_context("unable to preprocess markdown")?;

        Ok(Prepared {
            repository_use,
            theme,
            cache,
            repo_path,
            output_path,
            source_root: root_path,
            source_materialization,
        })
    }

    fn build(self) -> Result<(), Whatever> {
        zola::build(
            &self.theme,
            &self.cache,
            &self.repo_path,
            &self.output_path,
            self.repository_use.location.base_url.as_str(),
        )
        .whatever_context("zola build failed")?;
        Ok(())
    }

    fn serve(self) -> Result<(), Whatever> {
        let dirty_watcher = if self.source_materialization == git::SourceMaterialization::Dirty {
            Some(
                DirtyServeWatcher::start(self.source_root.clone(), self.repo_path.clone())
                    .whatever_context("unable to start dirty serve watcher")?,
            )
        } else {
            None
        };

        let result = zola::serve(&self.theme, &self.cache, &self.repo_path, &self.output_path)
            .whatever_context("zola serve failed");

        if let Some(dirty_watcher) = dirty_watcher {
            dirty_watcher.stop();
        }

        result
    }

    fn check(self) -> Result<(), Whatever> {
        zola::check(&self.theme, &self.cache, &self.repo_path)
            .whatever_context("zola check failed")?;
        Ok(())
    }
}

fn run_editorial_lint(
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

fn editorial_runtime_execution(
    resolved: &ResolvedExecution,
    selectors: &EditorialSelectorArgs,
) -> ResolvedExecution {
    let mut runtime = resolved.clone();
    if selectors.working_tree {
        runtime.source_materialization = git::SourceMaterialization::Dirty;
    }
    runtime
}

fn init_workspace(args: &Args, path: PathBuf, platform_dev: bool) -> Result<(), Whatever> {
    let root_path = root(args)?;
    let workspace_root = resolve_input_path(&path)?;
    std::fs::create_dir_all(&workspace_root)
        .whatever_context("unable to create workspace root directory")?;
    let workspace_root = workspace_root
        .canonicalize()
        .whatever_context("unable to canonicalize workspace root directory")?;

    // Workspace init is a local-dev bootstrap path, so it intentionally uses staging URLs.
    let workspace_config = Config::staging();
    let repository_use = workspace_config
        .locations
        .identify_repository(&root_path)
        .whatever_context("cannot identify repository use")?;

    let expected_root = workspace_root.join(&repository_use.title);
    if root_path != expected_root {
        snafu::whatever!(
            "workspace init expects the active repository at `{}`, found `{}`",
            expected_root.to_string_lossy(),
            root_path.to_string_lossy(),
        );
    }

    let (other_name, other_url) = repository_use
        .only_other_repo()
        .whatever_context("workspace init requires exactly one sibling repository")?;
    git::clone_missing_repo(other_url.as_str(), &workspace_root.join(other_name))
        .whatever_context("unable to clone workspace sibling repo")?;
    git::clone_missing_repo(
        workspace_config.theme.repository.as_str(),
        &workspace_root.join(config::DEFAULT_THEME_DIR),
    )
    .whatever_context("unable to clone workspace theme repo")?;

    if platform_dev {
        git::clone_missing_repo(
            PLATFORM_PREPROCESSOR_URL,
            &workspace_root.join("preprocessor"),
        )
        .whatever_context("unable to clone workspace preprocessor repo")?;
        git::clone_missing_repo(PLATFORM_EIPW_URL, &workspace_root.join("eipw"))
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

fn run() -> Result<(), Whatever> {
    let args = Args::parse();
    validate_non_execution_command_flags(&args)?;

    if let Operation::Print { print } = &args.operation {
        print::print(print.clone());
        return Ok(());
    }

    if let Operation::Workspace { command } = &args.operation {
        match command.clone() {
            WorkspaceCommand::Init { path, platform_dev } => {
                init_workspace(&args, path, platform_dev)?
            }
            WorkspaceCommand::Doctor => doctor_workspace(&args)?,
        }
        return Ok(());
    }

    let runtime_operation = args
        .operation
        .runtime_operation()
        .expect("non-execution commands should have returned earlier");
    let resolved = resolve_execution(&args)?;

    if matches!(runtime_operation, RuntimeOperation::Preview) {
        preview::serve(&output_path(&resolved.build_path))
            .whatever_context("preview server failed")?;
        return Ok(());
    }

    let build_path = make_build_dir(&resolved.build_path)?;
    let mut lock_file = lock(&build_path)?;

    match runtime_operation {
        RuntimeOperation::Clean => {
            // TODO: There's a race condition here. Maybe we move the lockfile to the repository
            //       root?
            lock_file
                .unlock()
                .whatever_context("unable to unlock build directory")?;
            std::fs::remove_dir_all(&build_path)
                .whatever_context("unable to remove build directory")?;
            return Ok(());
        }
        RuntimeOperation::Check => {
            Prepared::prepare(resolved)?.check()?;
        }
        RuntimeOperation::Build => {
            Prepared::prepare(resolved)?.build()?;
        }
        RuntimeOperation::Serve => {
            Prepared::prepare(resolved)?.serve()?;
        }
        RuntimeOperation::Preview => unreachable!(),
        RuntimeOperation::Changed { all, format } => {
            let repo_path = build_path.join(REPO_DIR);

            let both = git::Fresh::new(
                &resolved.root_path,
                &repo_path,
                resolved.repository_use.clone(),
                resolved.source_materialization,
            )
            .whatever_context("initializing build repo")?
            .clone_src()
            .whatever_context("cloning source repo")?
            .fetch_upstream()
            .whatever_context("fetching upstream repo")?;

            let changed_files: Vec<_> = both
                .changed_files()
                .whatever_context("unable to list changed files")?
                .into_iter()
                .filter(|p| all || is_proposal_path(p))
                .map(|p| repo_path.join(p))
                .collect();

            format.print(&changed_files, &repo_path);
        }
        RuntimeOperation::Editorial { command } => match command {
            EditorialCommand::Lint { selectors, eipw } => {
                run_editorial_lint(&resolved, &selectors, eipw)?;
            }
            EditorialCommand::Build { selectors, eipw } => {
                run_editorial_lint(&resolved, &selectors, eipw)?;
                Prepared::prepare(editorial_runtime_execution(&resolved, &selectors))?.check()?;
            }
        },
    }

    lock_file
        .unlock()
        .whatever_context("unable to unlock build directory")?;

    info!("build finished :3");
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use tempfile::TempDir;

    use super::{
        requested_profile_name, resolve_execution_settings, validate_non_execution_command_flags,
        Args, EditorialCommand, ExecutionSettings, Operation, ProfiledOperation, RuntimeOperation,
        SelectedSource, WorkspaceCommand,
    };
    use crate::config::{self, LoadedWorkspaceConfig};

    fn parse_args(arguments: &[&str]) -> Args {
        Args::try_parse_from(arguments).unwrap()
    }

    fn load_workspace_config(contents: &str) -> LoadedWorkspaceConfig {
        let workspace = TempDir::new().unwrap();
        let config_path = workspace.path().join(config::LOCAL_CONFIG_FILE);
        std::fs::write(&config_path, contents).unwrap();
        LoadedWorkspaceConfig::from_path(&config_path).unwrap()
    }

    fn selected_profile(
        args: &Args,
        workspace_config: Option<&LoadedWorkspaceConfig>,
    ) -> crate::config::SelectedProfile {
        let requested = requested_profile_name(args).unwrap();
        config::selected_profile(workspace_config, requested)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn profile_alias_parses_as_a_command_prefix() {
        let args = parse_args(&["build-eips", "parity", "build"]);

        assert!(matches!(
            args.operation,
            Operation::Parity {
                command: ProfiledOperation::Build
            }
        ));
        assert_eq!(
            requested_profile_name(&args).unwrap(),
            Some(config::PARITY_PROFILE)
        );
    }

    #[test]
    fn profile_alias_and_profile_flag_conflict() {
        let args = parse_args(&["build-eips", "--profile", "local", "parity", "build"]);
        let error = requested_profile_name(&args).unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot combine profile alias `parity` with `--profile local`"));
    }

    #[test]
    fn workspace_commands_reject_profile_selection() {
        let args = parse_args(&["build-eips", "--profile", "local", "workspace", "doctor"]);
        let error = validate_non_execution_command_flags(&args).unwrap_err();

        assert!(error
            .to_string()
            .contains("`--profile` cannot be used with `workspace` commands"));
    }

    #[test]
    fn command_groups_route_separately_from_profile_aliases() {
        let workspace = parse_args(&["build-eips", "workspace", "init", "/tmp/workspace"]);
        let doctor = parse_args(&["build-eips", "workspace", "doctor"]);
        let editorial_lint = parse_args(&[
            "build-eips",
            "--profile",
            "parity",
            "editorial",
            "lint",
            "--working-tree",
        ]);
        let editorial_build = parse_args(&[
            "build-eips",
            "--profile",
            "parity",
            "editorial",
            "build",
            "--working-tree",
        ]);

        assert!(matches!(
            workspace.operation,
            Operation::Workspace {
                command: WorkspaceCommand::Init { .. }
            }
        ));
        assert!(matches!(
            doctor.operation,
            Operation::Workspace {
                command: WorkspaceCommand::Doctor
            }
        ));
        assert!(matches!(
            editorial_lint.operation.runtime_operation(),
            Some(RuntimeOperation::Editorial {
                command: EditorialCommand::Lint { .. }
            })
        ));
        assert!(matches!(
            editorial_build.operation.runtime_operation(),
            Some(RuntimeOperation::Editorial {
                command: EditorialCommand::Build { .. }
            })
        ));
        assert!(validate_non_execution_command_flags(&editorial_lint).is_ok());
        assert_eq!(
            requested_profile_name(&editorial_lint).unwrap(),
            Some(config::PARITY_PROFILE)
        );
    }

    #[test]
    fn reserved_command_group_name_is_not_a_profile() {
        let args = parse_args(&["build-eips", "--profile", "editorial", "build"]);
        let requested = requested_profile_name(&args).unwrap();
        let error = config::selected_profile(None, requested).unwrap_err();

        assert!(error
            .to_string()
            .contains("reserved for a command group and cannot be selected"));
    }

    #[test]
    fn boolean_override_conflicts_are_hard_errors() {
        let args = parse_args(&["build-eips", "--staging", "--no-staging", "build"]);
        let error = resolve_execution_settings(&args, None, None).unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot pass both `--staging` and `--no-staging`"));
    }

    #[test]
    fn source_override_conflicts_are_hard_errors() {
        let args = parse_args(&[
            "build-eips",
            "--theme",
            "/tmp/theme",
            "--remote-theme",
            "build",
        ]);
        let error = resolve_execution_settings(&args, None, None).unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot pass both `--theme` and `--remote-theme`"));
    }

    #[test]
    fn dirty_profile_without_workspace_config_requires_explicit_resolution() {
        let args = parse_args(&["build-eips", "dirty", "build"]);
        let selected_profile = selected_profile(&args, None);
        let error = resolve_execution_settings(&args, None, Some(&selected_profile)).unwrap_err();
        let message = error.to_string();

        assert!(
            message.contains("profile `dirty` requires workspace-local theme and sibling sources")
        );
        assert!(message.contains("build-eips workspace init <workspace-root>"));
        assert!(message.contains("--theme <path>` and/or `--sibling-repo <path>"));
        assert!(message.contains("--remote-theme` and/or `--remote-sibling-repo"));
        assert!(message.contains("switch to `--profile parity`"));
    }

    #[test]
    fn dirty_with_remote_overrides_is_not_parity() {
        let dirty_args = parse_args(&[
            "build-eips",
            "--remote-theme",
            "--remote-sibling-repo",
            "dirty",
            "build",
        ]);
        let dirty_profile = selected_profile(&dirty_args, None);
        let dirty_settings =
            resolve_execution_settings(&dirty_args, None, Some(&dirty_profile)).unwrap();

        let parity_args = parse_args(&["build-eips", "parity", "build"]);
        let parity_profile = selected_profile(&parity_args, None);
        let parity_settings =
            resolve_execution_settings(&parity_args, None, Some(&parity_profile)).unwrap();

        assert_eq!(
            dirty_settings,
            ExecutionSettings {
                build_root: None,
                staging: true,
                allow_dirty: true,
                theme: SelectedSource::Remote,
                sibling: SelectedSource::Remote,
            }
        );
        assert_eq!(
            parity_settings,
            ExecutionSettings {
                build_root: None,
                staging: true,
                allow_dirty: false,
                theme: SelectedSource::Remote,
                sibling: SelectedSource::Remote,
            }
        );
    }

    #[test]
    fn default_profile_selection_and_overrides_share_the_same_path() {
        let workspace_config = load_workspace_config(
            r#"
default_profile = "local"

[profiles.local]
staging = true
theme = "local"
sibling = "local"
"#,
        );
        let args = parse_args(&["build-eips", "--no-staging", "--remote-theme", "build"]);
        let selected_profile = config::selected_profile(
            Some(&workspace_config),
            requested_profile_name(&args).unwrap(),
        )
        .unwrap()
        .unwrap();
        let settings =
            resolve_execution_settings(&args, Some(&workspace_config), Some(&selected_profile))
                .unwrap();

        assert_eq!(selected_profile.name, "local");
        assert_eq!(
            settings,
            ExecutionSettings {
                build_root: None,
                staging: false,
                allow_dirty: false,
                theme: SelectedSource::Remote,
                sibling: SelectedSource::WorkspaceLocal,
            }
        );
    }

    #[test]
    fn built_in_profile_can_be_selected_through_profile_flag_without_workspace_config() {
        let args = parse_args(&["build-eips", "--profile", "dirty", "build"]);
        let selected_profile = selected_profile(&args, None);

        assert_eq!(selected_profile.name, config::DIRTY_PROFILE);
    }
}

fn main() -> Result<(), Report<Whatever>> {
    let logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let level = logger.filter();
    progress::init(logger);
    log::set_max_level(level);

    let result = run().map_err(Report::from_error);

    progress::clear();

    result
}
