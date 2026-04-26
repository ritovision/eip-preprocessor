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

use crate::config::{Config, LoadedRepoManifest, LoadedWorkspaceConfig, ServerBinding};

const CONTENT_DIR: &str = "content";
const BUILD_DIR: &str = "build";
const REPO_DIR: &str = "repo";
const OUTPUT_DIR: &str = "output";
const PROPOSAL_TEMPLATE_URL: &str = "https://github.com/eips-wg/template.git";
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

    /// Force the staging repositories and base URLs
    #[clap(long)]
    staging: bool,

    /// Force the production repositories and base URLs
    #[clap(long)]
    production: bool,

    /// Use the configured remote theme instead of a workspace-local theme
    #[clap(long)]
    remote_theme: bool,

    /// Use the configured remote sibling content repository
    #[clap(long)]
    remote_sibling_repo: bool,

    /// Write build artifacts under BUILD_ROOT instead of the default location
    #[clap(long)]
    build_root: Option<PathBuf>,

    #[clap(subcommand)]
    operation: Operation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
struct ServerCliArgs {
    /// Host/interface for the local server to bind
    #[arg(long)]
    host: Option<String>,

    /// Port for the local server to bind
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
struct BaseUrlCliArgs {
    /// Override the rendered-site base URL for this command
    #[arg(long, value_parser = clap::value_parser!(Url))]
    base_url: Option<Url>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
struct CleanCliArgs {
    /// Ignore tracked working-tree changes in the active repo
    #[arg(long)]
    clean: bool,
}

#[derive(Debug, Clone, Subcommand)]
enum Operation {
    /// Print various useful things, like available lints
    Print {
        #[command(flatten)]
        print: print::CmdArgs,
    },

    /// Build the project and output HTML
    Build {
        #[command(flatten)]
        base_url: BaseUrlCliArgs,

        #[command(flatten)]
        clean: CleanCliArgs,
    },

    /// Build the project and launch a web server to preview it
    Serve {
        #[command(flatten)]
        server: ServerCliArgs,

        #[command(flatten)]
        base_url: BaseUrlCliArgs,

        #[command(flatten)]
        clean: CleanCliArgs,
    },

    /// Serve the existing built output without rebuilding it
    Preview {
        #[command(flatten)]
        server: ServerCliArgs,
    },

    /// Remove temporary and output files
    Clean,

    /// Analyze the repository and report errors, but don't build HTML files
    Check {
        #[command(flatten)]
        clean: CleanCliArgs,
    },

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

    /// Run a normal command with the built-in parity mode
    Parity {
        #[command(subcommand)]
        command: ProfiledOperation,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum ProfiledOperation {
    /// Build the project and output HTML
    Build {
        #[command(flatten)]
        base_url: BaseUrlCliArgs,
    },

    /// Build the project and launch a web server to preview it
    Serve {
        #[command(flatten)]
        server: ServerCliArgs,

        #[command(flatten)]
        base_url: BaseUrlCliArgs,
    },

    /// Analyze the repository and report errors, but don't build HTML files
    Check,
}

#[derive(Debug, Subcommand, Clone)]
enum WorkspaceCommand {
    /// Create the local workspace config and clone any missing sibling repositories
    Init {
        /// Workspace root directory
        path: PathBuf,

        /// Also clone template for proposal-family scaffold work
        #[arg(long)]
        template: bool,

        /// Also clone preprocessor and eipw for platform development
        #[arg(long)]
        platform_dev: bool,
    },

    /// Check whether the local workspace bootstrap is ready for direct build-eips commands
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
    server_binding: ServerBinding,
    base_url_override: Option<Url>,
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

struct WorkspaceInitRepositories<'a> {
    theme: &'a Url,
    template: &'a Url,
    preprocessor: &'a Url,
    eipw: &'a Url,
}

#[derive(Debug, Clone)]
enum ActiveRepoIdentity {
    Manifest(Box<LoadedRepoManifest>),
    Legacy { repo_id: String },
}

impl ActiveRepoIdentity {
    fn load(root_path: &Path) -> Result<Self, Whatever> {
        if let Some(manifest) =
            LoadedRepoManifest::load(root_path).whatever_context("unable to load repo manifest")?
        {
            return Ok(Self::Manifest(Box::new(manifest)));
        }

        match Config::production()
            .locations
            .identify_repository_title(root_path)
        {
            Ok(repo_id) => Ok(Self::Legacy { repo_id }),
            Err(git::Error::NoIdentify { .. }) => {
                snafu::whatever!(
                    "active repository `{}` does not carry `{}` and does not match the legacy EIPs/ERCs identity fallback",
                    root_path.to_string_lossy(),
                    config::REPO_MANIFEST_FILE
                )
            }
            Err(error) => Err(error).whatever_context("cannot identify legacy repository use"),
        }
    }

    fn repo_id(&self) -> &str {
        match self {
            Self::Manifest(manifest) => &manifest.manifest().repo_id,
            Self::Legacy { repo_id } => repo_id,
        }
    }

    fn source_description(&self) -> &'static str {
        match self {
            Self::Manifest(_) => "repo manifest",
            Self::Legacy { .. } => "legacy EIPs/ERCs fallback",
        }
    }

    fn manifest(&self) -> Option<&LoadedRepoManifest> {
        match self {
            Self::Manifest(manifest) => Some(manifest.as_ref()),
            Self::Legacy { .. } => None,
        }
    }

    fn sibling_ids(&self) -> Vec<String> {
        match self {
            Self::Manifest(manifest) => manifest.manifest().siblings.keys().cloned().collect(),
            Self::Legacy { repo_id } => Config::production()
                .locations
                .repository_use_for_title(repo_id)
                .expect("legacy repository id should have metadata")
                .other_repos
                .keys()
                .cloned()
                .collect(),
        }
    }

    fn repository_use(&self, staging: bool) -> Result<git::RepositoryUse, Whatever> {
        match self {
            Self::Manifest(manifest) => {
                let manifest = manifest.manifest();
                Ok(git::RepositoryUse {
                    title: manifest.repo_id.clone(),
                    location: manifest.active_endpoint(staging),
                    other_repos: manifest.sibling_repositories(staging),
                })
            }
            Self::Legacy { repo_id } => {
                let baseline = if staging {
                    Config::staging()
                } else {
                    Config::production()
                };
                baseline
                    .locations
                    .repository_use_for_title(repo_id)
                    .with_whatever_context(|| {
                        format!("legacy repository metadata for `{repo_id}` is unavailable")
                    })
            }
        }
    }
}

impl Operation {
    fn server_cli_args(&self) -> ServerCliArgs {
        match self {
            Self::Serve { server, .. } | Self::Preview { server } => server.clone(),
            Self::Parity { command } => command.server_cli_args(),
            Self::Print { .. }
            | Self::Build { .. }
            | Self::Clean
            | Self::Check { .. }
            | Self::Changed { .. }
            | Self::Editorial { .. }
            | Self::Workspace { .. } => ServerCliArgs::default(),
        }
    }

    fn base_url_cli_args(&self) -> BaseUrlCliArgs {
        match self {
            Self::Build { base_url, .. } | Self::Serve { base_url, .. } => base_url.clone(),
            Self::Parity { command } => command.base_url_cli_args(),
            Self::Print { .. }
            | Self::Preview { .. }
            | Self::Clean
            | Self::Check { .. }
            | Self::Changed { .. }
            | Self::Editorial { .. }
            | Self::Workspace { .. } => BaseUrlCliArgs::default(),
        }
    }

    fn clean_cli_args(&self) -> CleanCliArgs {
        match self {
            Self::Build { clean, .. } | Self::Serve { clean, .. } | Self::Check { clean } => {
                clean.clone()
            }
            Self::Print { .. }
            | Self::Preview { .. }
            | Self::Clean
            | Self::Changed { .. }
            | Self::Editorial { .. }
            | Self::Workspace { .. }
            | Self::Parity { .. } => CleanCliArgs::default(),
        }
    }

    fn is_plain_site_command(&self) -> bool {
        matches!(
            self,
            Self::Build { .. } | Self::Serve { .. } | Self::Check { .. }
        )
    }

    fn is_editorial_build_command(&self) -> bool {
        matches!(
            self,
            Self::Editorial {
                command: EditorialCommand::Build { .. }
            }
        )
    }

    fn runtime_operation(&self) -> Option<RuntimeOperation> {
        match self {
            Self::Print { .. } | Self::Workspace { .. } => None,
            Self::Build { .. } => Some(RuntimeOperation::Build),
            Self::Serve { .. } => Some(RuntimeOperation::Serve),
            Self::Preview { .. } => Some(RuntimeOperation::Preview),
            Self::Clean => Some(RuntimeOperation::Clean),
            Self::Check { .. } => Some(RuntimeOperation::Check),
            Self::Changed { all, format } => Some(RuntimeOperation::Changed {
                all: *all,
                format: format.clone(),
            }),
            Self::Editorial { command } => Some(RuntimeOperation::Editorial {
                command: command.clone(),
            }),
            Self::Parity { command } => Some(command.runtime_operation()),
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
    fn server_cli_args(&self) -> ServerCliArgs {
        match self {
            Self::Serve { server, .. } => server.clone(),
            Self::Build { .. } | Self::Check => ServerCliArgs::default(),
        }
    }

    fn base_url_cli_args(&self) -> BaseUrlCliArgs {
        match self {
            Self::Build { base_url } | Self::Serve { base_url, .. } => base_url.clone(),
            Self::Check => BaseUrlCliArgs::default(),
        }
    }

    fn runtime_operation(&self) -> RuntimeOperation {
        match self {
            Self::Build { .. } => RuntimeOperation::Build,
            Self::Serve { .. } => RuntimeOperation::Serve,
            Self::Check => RuntimeOperation::Check,
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
    let config_path = config::discover_path(&search_from);

    Ok(WorkspaceCommandContext {
        search_from,
        config_path,
    })
}

fn has_execution_override_flags(args: &Args) -> bool {
    args.staging
        || args.production
        || args.remote_theme
        || args.remote_sibling_repo
        || args.build_root.is_some()
}

fn validate_non_execution_command_flags(args: &Args) -> Result<(), Whatever> {
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

fn explicit_environment_or_parity(args: &Args) -> Result<Option<bool>, Whatever> {
    if let Some(staging) = resolve_environment_override(args)? {
        return Ok(Some(staging));
    }

    if matches!(args.operation, Operation::Parity { .. }) {
        return Ok(Some(true));
    }

    Ok(None)
}

fn resolve_execution_settings(
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

fn collect_doctor_report(args: &Args, check_tools: bool) -> Result<DoctorReport, Whatever> {
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

fn doctor_workspace(args: &Args) -> Result<(), Whatever> {
    let report = collect_doctor_report(args, true)?;

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

fn output_path(build_path: &Path) -> PathBuf {
    build_path.join(OUTPUT_DIR)
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

fn resolve_server_binding(
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

fn resolve_base_url_override(
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

fn resolve_execution(args: &Args) -> Result<ResolvedExecution, Whatever> {
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
    server_binding: ServerBinding,
    base_url_override: Option<Url>,
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
            server_binding,
            base_url_override,
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
            server_binding,
            base_url_override,
        })
    }

    fn build(self) -> Result<(), Whatever> {
        let base_url = self
            .base_url_override
            .as_ref()
            .unwrap_or(&self.repository_use.location.base_url);
        zola::build(
            &self.theme,
            &self.cache,
            &self.repo_path,
            &self.output_path,
            base_url.as_str(),
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

        let result = zola::serve(
            &self.theme,
            &self.cache,
            &self.repo_path,
            &self.output_path,
            &self.server_binding,
            self.base_url_override.as_ref(),
        )
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

fn init_workspace(
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

fn init_workspace_with_repositories(
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

fn run() -> Result<(), Whatever> {
    let args = Args::parse();
    validate_non_execution_command_flags(&args)?;

    if let Operation::Print { print } = &args.operation {
        print::print(print.clone());
        return Ok(());
    }

    if let Operation::Workspace { command } = &args.operation {
        match command.clone() {
            WorkspaceCommand::Init {
                path,
                template,
                platform_dev,
            } => init_workspace(&args, path, template, platform_dev)?,
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
        preview::serve(&output_path(&resolved.build_path), &resolved.server_binding)
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
    use std::path::{Path, PathBuf};

    use clap::Parser;
    use git2::{IndexAddOption, Repository, Signature};
    use snafu::Report;
    use tempfile::TempDir;
    use url::Url;

    use super::{
        collect_doctor_report, editorial_runtime_execution, editorial_targets,
        explicit_environment_or_parity, init_workspace_with_repositories,
        resolve_base_url_override, resolve_execution, resolve_execution_settings,
        resolve_server_binding, validate_non_execution_command_flags, Args, EditorialCommand,
        EditorialSelectorArgs, ExecutionSettings, Operation, ProfiledOperation, ResolvedExecution,
        RuntimeOperation, SelectedSource, ServerCliArgs, ThemeSource, WorkspaceCommand,
        WorkspaceInitRepositories, REPO_DIR,
    };
    use crate::config::{self, LoadedWorkspaceConfig, ServerBinding};

    fn parse_args(arguments: &[&str]) -> Args {
        Args::try_parse_from(arguments).unwrap()
    }

    fn load_workspace_config(contents: &str) -> LoadedWorkspaceConfig {
        let workspace = TempDir::new().unwrap();
        let config_path = workspace.path().join(config::LOCAL_CONFIG_FILE);
        std::fs::write(&config_path, contents).unwrap();
        LoadedWorkspaceConfig::from_path(&config_path).unwrap()
    }

    fn settings_for(
        arguments: &[&str],
        sibling_ids: &[&str],
        workspace_config: Option<&LoadedWorkspaceConfig>,
    ) -> ExecutionSettings {
        let args = parse_args(arguments);
        let sibling_ids = sibling_ids
            .iter()
            .map(|sibling_id| (*sibling_id).to_owned())
            .collect::<Vec<_>>();

        resolve_execution_settings(&args, &sibling_ids, workspace_config).unwrap()
    }

    fn assert_settings(
        arguments: &[&str],
        sibling_ids: &[&str],
        workspace_config: Option<&LoadedWorkspaceConfig>,
        expected: ExecutionSettings,
    ) {
        assert_eq!(
            settings_for(arguments, sibling_ids, workspace_config),
            expected
        );
    }

    fn file_url(path: &Path) -> Url {
        Url::from_directory_path(path).unwrap()
    }

    fn write_file(root: &Path, relative: impl AsRef<Path>, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn commit_all(repo: &Repository, message: &str) {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let signature = Signature::now("build-eips test", "build-eips@example.test").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repo.find_commit(oid).unwrap())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap();
    }

    fn init_repo(path: &Path, files: &[(&str, &str)]) -> Repository {
        std::fs::create_dir_all(path).unwrap();
        let repo = Repository::init(path).unwrap();
        repo.set_head("refs/heads/master").unwrap();
        for (relative, contents) in files {
            write_file(path, relative, contents);
        }
        commit_all(&repo, "initial");
        repo
    }

    fn append_and_commit(repo: &Repository, root: &Path, files: &[(&str, &str)], message: &str) {
        for (relative, contents) in files {
            write_file(root, relative, contents);
        }
        commit_all(repo, message);
    }

    fn repo_manifest_text(repo_id: &str, repository: &Url, siblings: &[(&str, Url)]) -> String {
        let mut manifest = format!(
            r#"
repo_id = "{repo_id}"

[production]
repository = "{repository}"
base_url = "https://example.test/{repo_id}/"

[staging]
repository = "{repository}"
base_url = "https://staging.example.test/{repo_id}/"
"#
        );

        for (sibling_id, sibling_repository) in siblings {
            manifest.push_str(&format!(
                r#"
[siblings.{sibling_id}.production]
repository = "{sibling_repository}"
base_url = "https://example.test/{sibling_id}/"

[siblings.{sibling_id}.staging]
repository = "{sibling_repository}"
base_url = "https://staging.example.test/{sibling_id}/"
"#
            ));
        }

        manifest
    }

    fn write_repo_manifest_file(
        path: &Path,
        repo_id: &str,
        upstream: &Url,
        siblings: &[(&str, Url)],
    ) {
        write_file(
            path,
            config::REPO_MANIFEST_FILE,
            &repo_manifest_text(repo_id, upstream, siblings),
        );
    }

    fn write_manifest_repo(
        path: &Path,
        repo_id: &str,
        upstream: &Url,
        siblings: &[(&str, Url)],
    ) -> Repository {
        let repo = init_repo(path, &[("content/0001.md", "# Proposal\n")]);
        write_repo_manifest_file(path, repo_id, upstream, siblings);
        commit_all(&repo, "add repo manifest");
        repo
    }

    #[test]
    fn parity_command_parses_as_command_prefix() {
        let args = parse_args(&["build-eips", "parity", "build"]);

        assert!(matches!(
            args.operation,
            Operation::Parity {
                command: ProfiledOperation::Build { .. }
            }
        ));
    }

    #[test]
    fn profile_flag_is_rejected() {
        let error =
            Args::try_parse_from(["build-eips", "--profile", "local", "build"]).unwrap_err();

        assert!(error
            .to_string()
            .contains("unexpected argument '--profile'"));
    }

    #[test]
    fn command_groups_route_separately_from_parity() {
        let workspace = parse_args(&["build-eips", "workspace", "init", "/tmp/workspace"]);
        let doctor = parse_args(&["build-eips", "workspace", "doctor"]);
        let editorial_lint = parse_args(&["build-eips", "editorial", "lint", "--working-tree"]);
        let editorial_build = parse_args(&["build-eips", "editorial", "build", "--working-tree"]);

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
    }

    #[test]
    fn server_flags_parse_on_serve_and_preview_forms() {
        let cases: &[(&[&str], bool)] = &[
            (
                &["build-eips", "serve", "--host", "0.0.0.0", "--port", "8080"],
                true,
            ),
            (
                &[
                    "build-eips",
                    "preview",
                    "--host",
                    "0.0.0.0",
                    "--port",
                    "8080",
                ],
                false,
            ),
            (
                &[
                    "build-eips",
                    "parity",
                    "serve",
                    "--host",
                    "0.0.0.0",
                    "--port",
                    "8080",
                ],
                true,
            ),
        ];

        for (arguments, expect_serve) in cases {
            let args = parse_args(arguments);
            let runtime_operation = args.operation.runtime_operation().unwrap();
            match runtime_operation {
                RuntimeOperation::Serve if *expect_serve => {}
                RuntimeOperation::Preview if !*expect_serve => {}
                other => panic!("unexpected runtime operation: {other:?}"),
            }
            let server = args.operation.server_cli_args();

            assert_eq!(server.host.as_deref(), Some("0.0.0.0"));
            assert_eq!(server.port, Some(8080));
        }
    }

    #[test]
    fn server_binding_resolution_uses_cli_config_then_defaults() {
        assert_eq!(
            resolve_server_binding(None, &ServerCliArgs::default()),
            ServerBinding {
                host: "127.0.0.1".to_owned(),
                port: 1111,
            }
        );

        let workspace_config = load_workspace_config(
            r#"
[server]
host = "0.0.0.0"
port = 8080
"#,
        );

        assert_eq!(
            resolve_server_binding(Some(&workspace_config), &ServerCliArgs::default()),
            ServerBinding {
                host: "0.0.0.0".to_owned(),
                port: 8080,
            }
        );
        assert_eq!(
            resolve_server_binding(
                Some(&workspace_config),
                &ServerCliArgs {
                    host: Some("127.0.0.1".to_owned()),
                    port: Some(4000),
                },
            ),
            ServerBinding {
                host: "127.0.0.1".to_owned(),
                port: 4000,
            }
        );
        assert_eq!(
            resolve_server_binding(
                Some(&workspace_config),
                &ServerCliArgs {
                    host: None,
                    port: Some(4000),
                },
            ),
            ServerBinding {
                host: "0.0.0.0".to_owned(),
                port: 4000,
            }
        );
    }

    #[test]
    fn base_url_flags_parse_on_build_and_serve_forms() {
        let cases: &[(&[&str], RuntimeOperation)] = &[
            (
                &["build-eips", "build", "--base-url", "http://localhost:4000"],
                RuntimeOperation::Build,
            ),
            (
                &["build-eips", "serve", "--base-url", "http://localhost:4000"],
                RuntimeOperation::Serve,
            ),
            (
                &[
                    "build-eips",
                    "parity",
                    "build",
                    "--base-url",
                    "http://localhost:4000",
                ],
                RuntimeOperation::Build,
            ),
            (
                &[
                    "build-eips",
                    "parity",
                    "serve",
                    "--base-url",
                    "http://localhost:4000",
                ],
                RuntimeOperation::Serve,
            ),
        ];

        for (arguments, expected_runtime_operation) in cases {
            let args = parse_args(arguments);

            assert!(matches!(
                (
                    args.operation.runtime_operation().unwrap(),
                    (*expected_runtime_operation).clone()
                ),
                (RuntimeOperation::Build, RuntimeOperation::Build)
                    | (RuntimeOperation::Serve, RuntimeOperation::Serve)
            ));
            assert_eq!(
                args.operation
                    .base_url_cli_args()
                    .base_url
                    .as_ref()
                    .unwrap()
                    .as_str(),
                "http://localhost:4000/"
            );
        }
    }

    #[test]
    fn clean_flags_parse_only_on_plain_site_commands() {
        for arguments in [
            &["build-eips", "build", "--clean"][..],
            &["build-eips", "serve", "--clean"][..],
            &["build-eips", "check", "--clean"][..],
        ] {
            let args = parse_args(arguments);
            assert!(args.operation.clean_cli_args().clean);
        }

        for arguments in [
            &["build-eips", "parity", "build", "--clean"][..],
            &["build-eips", "parity", "serve", "--clean"][..],
            &["build-eips", "parity", "check", "--clean"][..],
            &["build-eips", "preview", "--clean"][..],
            &["build-eips", "changed", "--clean"][..],
            &["build-eips", "clean", "--clean"][..],
        ] {
            assert!(Args::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn removed_dirty_command_surface_is_rejected() {
        for arguments in [
            &["build-eips", "dirty", "build"][..],
            &["build-eips", "--allow-dirty", "build"][..],
            &["build-eips", "--no-allow-dirty", "build"][..],
            &["build-eips", "--no-staging", "build"][..],
            &["build-eips", "parity", "preview"][..],
            &["build-eips", "parity", "clean"][..],
            &["build-eips", "parity", "changed"][..],
        ] {
            assert!(Args::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn base_url_flag_is_rejected_on_non_rendering_forms() {
        let cases: &[&[&str]] = &[
            &[
                "build-eips",
                "preview",
                "--base-url",
                "http://localhost:4000",
            ],
            &[
                "build-eips",
                "parity",
                "preview",
                "--base-url",
                "http://localhost:4000",
            ],
            &["build-eips", "check", "--base-url", "http://localhost:4000"],
            &[
                "build-eips",
                "changed",
                "--base-url",
                "http://localhost:4000",
            ],
            &[
                "build-eips",
                "workspace",
                "doctor",
                "--base-url",
                "http://localhost:4000",
            ],
            &[
                "build-eips",
                "workspace",
                "init",
                "/tmp/workspace",
                "--base-url",
                "http://localhost:4000",
            ],
            &[
                "build-eips",
                "editorial",
                "lint",
                "--working-tree",
                "--base-url",
                "http://localhost:4000",
            ],
            &["build-eips", "print", "--base-url", "http://localhost:4000"],
        ];

        for arguments in cases {
            assert!(Args::try_parse_from(*arguments).is_err());
        }
    }

    #[test]
    fn explicit_env_or_parity_provenance_is_classified_separately_from_local_defaults() {
        let cases: &[(&[&str], Option<bool>)] = &[
            (&["build-eips", "--staging", "build"], Some(true)),
            (&["build-eips", "--production", "build"], Some(false)),
            (&["build-eips", "parity", "build"], Some(true)),
            (&["build-eips", "build"], None),
            (&["build-eips", "serve"], None),
            (&["build-eips", "check"], None),
        ];

        for (arguments, expected) in cases {
            let args = parse_args(arguments);
            assert_eq!(explicit_environment_or_parity(&args).unwrap(), *expected);
        }
    }

    #[test]
    fn base_url_override_resolution_uses_cli_config_then_provenance() {
        let workspace_config = load_workspace_config(
            r#"
[site]
base_url = "http://localhost:4000"
"#,
        );
        let none = parse_args(&["build-eips", "build"]);
        assert!(resolve_base_url_override(&none, None).unwrap().is_none());

        for arguments in [&["build-eips", "build"][..], &["build-eips", "serve"][..]] {
            let args = parse_args(arguments);
            assert_eq!(
                resolve_base_url_override(&args, Some(&workspace_config))
                    .unwrap()
                    .unwrap()
                    .as_str(),
                "http://localhost:4000/"
            );
        }

        let cli = parse_args(&["build-eips", "build", "--base-url", "http://localhost:5000"]);
        assert_eq!(
            resolve_base_url_override(&cli, Some(&workspace_config))
                .unwrap()
                .unwrap()
                .as_str(),
            "http://localhost:5000/"
        );

        for arguments in [
            &["build-eips", "--staging", "build"][..],
            &["build-eips", "--production", "build"][..],
            &["build-eips", "parity", "build"][..],
            &["build-eips", "parity", "serve"][..],
        ] {
            let args = parse_args(arguments);
            assert!(resolve_base_url_override(&args, Some(&workspace_config))
                .unwrap()
                .is_none());
        }

        for arguments in [
            &[
                "build-eips",
                "--staging",
                "build",
                "--base-url",
                "http://localhost:5000",
            ][..],
            &[
                "build-eips",
                "--production",
                "build",
                "--base-url",
                "http://localhost:5000",
            ][..],
            &[
                "build-eips",
                "parity",
                "build",
                "--base-url",
                "http://localhost:5000",
            ][..],
            &[
                "build-eips",
                "parity",
                "serve",
                "--base-url",
                "http://localhost:5000",
            ][..],
        ] {
            let args = parse_args(arguments);
            assert_eq!(
                resolve_base_url_override(&args, Some(&workspace_config))
                    .unwrap()
                    .unwrap()
                    .as_str(),
                "http://localhost:5000/"
            );
        }
    }

    #[test]
    fn local_site_base_url_override_does_not_change_execution_settings() {
        let workspace_config = load_workspace_config(
            r#"
[site]
base_url = "http://localhost:4000"
"#,
        );
        let args = parse_args(&["build-eips", "build"]);
        let settings = resolve_execution_settings(&args, &[], Some(&workspace_config)).unwrap();

        assert_eq!(
            resolve_base_url_override(&args, Some(&workspace_config))
                .unwrap()
                .unwrap()
                .as_str(),
            "http://localhost:4000/"
        );
        assert_eq!(
            settings,
            ExecutionSettings {
                build_root: None,
                staging: true,
                allow_dirty: true,
                theme: SelectedSource::WorkspaceLocal,
                sibling: SelectedSource::WorkspaceLocal,
            }
        );
    }

    #[test]
    fn plain_site_commands_are_local_first_dirty_staging() {
        let workspace_config = load_workspace_config("");
        let expected = ExecutionSettings {
            build_root: None,
            staging: true,
            allow_dirty: true,
            theme: SelectedSource::WorkspaceLocal,
            sibling: SelectedSource::WorkspaceLocal,
        };

        for arguments in [
            &["build-eips", "build"][..],
            &["build-eips", "serve"][..],
            &["build-eips", "check"][..],
        ] {
            assert_settings(
                arguments,
                &["ERCs"],
                Some(&workspace_config),
                expected.clone(),
            );
        }
    }

    #[test]
    fn clean_plain_site_commands_keep_local_sources_but_disable_dirty_materialization() {
        let workspace_config = load_workspace_config("");
        let expected = ExecutionSettings {
            build_root: None,
            staging: true,
            allow_dirty: false,
            theme: SelectedSource::WorkspaceLocal,
            sibling: SelectedSource::WorkspaceLocal,
        };

        for arguments in [
            &["build-eips", "build", "--clean"][..],
            &["build-eips", "serve", "--clean"][..],
            &["build-eips", "check", "--clean"][..],
        ] {
            assert_settings(
                arguments,
                &["ERCs"],
                Some(&workspace_config),
                expected.clone(),
            );
        }
    }

    #[test]
    fn explicit_environment_site_commands_are_remote_clean_without_workspace_config() {
        for (arguments, expected_staging) in [
            (&["build-eips", "--staging", "build"][..], true),
            (&["build-eips", "--staging", "serve"][..], true),
            (&["build-eips", "--staging", "check"][..], true),
            (&["build-eips", "--production", "build"][..], false),
            (&["build-eips", "--production", "serve"][..], false),
            (&["build-eips", "--production", "check"][..], false),
        ] {
            assert_settings(
                arguments,
                &["ERCs"],
                None,
                ExecutionSettings {
                    build_root: None,
                    staging: expected_staging,
                    allow_dirty: false,
                    theme: SelectedSource::Remote,
                    sibling: SelectedSource::Remote,
                },
            );
        }
    }

    #[test]
    fn clean_environment_commands_are_accepted_as_redundant_remote_clean() {
        for (arguments, expected_staging) in [
            (&["build-eips", "--staging", "build", "--clean"][..], true),
            (&["build-eips", "--staging", "serve", "--clean"][..], true),
            (&["build-eips", "--staging", "check", "--clean"][..], true),
            (
                &["build-eips", "--production", "build", "--clean"][..],
                false,
            ),
            (
                &["build-eips", "--production", "serve", "--clean"][..],
                false,
            ),
            (
                &["build-eips", "--production", "check", "--clean"][..],
                false,
            ),
        ] {
            assert_settings(
                arguments,
                &["ERCs"],
                None,
                ExecutionSettings {
                    build_root: None,
                    staging: expected_staging,
                    allow_dirty: false,
                    theme: SelectedSource::Remote,
                    sibling: SelectedSource::Remote,
                },
            );
        }
    }

    #[test]
    fn remote_source_overrides_compose_with_local_dirty_and_clean_modes() {
        let workspace_config = load_workspace_config("");
        let cases = [
            (
                &["build-eips", "--remote-theme", "build"][..],
                true,
                SelectedSource::Remote,
                SelectedSource::WorkspaceLocal,
            ),
            (
                &["build-eips", "--remote-sibling-repo", "build"][..],
                true,
                SelectedSource::WorkspaceLocal,
                SelectedSource::Remote,
            ),
            (
                &["build-eips", "--remote-theme", "build", "--clean"][..],
                false,
                SelectedSource::Remote,
                SelectedSource::WorkspaceLocal,
            ),
            (
                &["build-eips", "--remote-sibling-repo", "build", "--clean"][..],
                false,
                SelectedSource::WorkspaceLocal,
                SelectedSource::Remote,
            ),
        ];

        for (arguments, allow_dirty, theme, sibling) in cases {
            assert_settings(
                arguments,
                &["ERCs"],
                Some(&workspace_config),
                ExecutionSettings {
                    build_root: None,
                    staging: true,
                    allow_dirty,
                    theme,
                    sibling,
                },
            );
        }
    }

    #[test]
    fn non_site_commands_do_not_require_workspace_local_sources() {
        for arguments in [
            &["build-eips", "changed"][..],
            &["build-eips", "clean"][..],
            &["build-eips", "preview"][..],
            &["build-eips", "editorial", "lint", "content/0001.md"][..],
        ] {
            assert_settings(
                arguments,
                &["ERCs"],
                None,
                ExecutionSettings {
                    build_root: None,
                    staging: false,
                    allow_dirty: false,
                    theme: SelectedSource::Remote,
                    sibling: SelectedSource::Remote,
                },
            );
        }
    }

    #[test]
    fn changed_environment_flags_use_remote_clean_metadata_without_workspace_config() {
        for (arguments, expected_staging) in [
            (&["build-eips", "--staging", "changed"][..], true),
            (&["build-eips", "--production", "changed"][..], false),
        ] {
            assert_settings(
                arguments,
                &["ERCs"],
                None,
                ExecutionSettings {
                    build_root: None,
                    staging: expected_staging,
                    allow_dirty: false,
                    theme: SelectedSource::Remote,
                    sibling: SelectedSource::Remote,
                },
            );
        }
    }

    #[test]
    fn editorial_dispatch_uses_local_first_only_for_runtime_build_path() {
        let workspace_config = load_workspace_config("");

        assert_settings(
            &["build-eips", "editorial", "lint", "content/0001.md"],
            &["ERCs"],
            None,
            ExecutionSettings {
                build_root: None,
                staging: false,
                allow_dirty: false,
                theme: SelectedSource::Remote,
                sibling: SelectedSource::Remote,
            },
        );
        assert_settings(
            &["build-eips", "editorial", "build", "content/0001.md"],
            &["ERCs"],
            Some(&workspace_config),
            ExecutionSettings {
                build_root: None,
                staging: true,
                allow_dirty: true,
                theme: SelectedSource::WorkspaceLocal,
                sibling: SelectedSource::WorkspaceLocal,
            },
        );
        assert_settings(
            &[
                "build-eips",
                "--staging",
                "editorial",
                "build",
                "--against-upstream",
            ],
            &["ERCs"],
            None,
            ExecutionSettings {
                build_root: None,
                staging: true,
                allow_dirty: false,
                theme: SelectedSource::Remote,
                sibling: SelectedSource::Remote,
            },
        );
        assert_settings(
            &[
                "build-eips",
                "--production",
                "editorial",
                "build",
                "--against-upstream",
            ],
            &["ERCs"],
            None,
            ExecutionSettings {
                build_root: None,
                staging: false,
                allow_dirty: false,
                theme: SelectedSource::Remote,
                sibling: SelectedSource::Remote,
            },
        );
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
            theme: ThemeSource::Remote {
                repository: "https://example.test/theme.git".to_owned(),
                commit: "HEAD".to_owned(),
            },
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
            editorial_runtime_execution(&resolved, &selectors).source_materialization,
            crate::git::SourceMaterialization::Dirty
        );
    }

    #[test]
    fn parity_site_commands_remain_remote_clean_staging() {
        for arguments in [
            &["build-eips", "parity", "build"][..],
            &["build-eips", "parity", "serve"][..],
            &["build-eips", "parity", "check"][..],
        ] {
            assert_settings(
                arguments,
                &["ERCs"],
                None,
                ExecutionSettings {
                    build_root: None,
                    staging: true,
                    allow_dirty: false,
                    theme: SelectedSource::Remote,
                    sibling: SelectedSource::Remote,
                },
            );
        }
    }

    #[test]
    fn downstream_ci_environment_forms_do_not_need_workspace_config() {
        for (arguments, expected_staging) in [
            (&["build-eips", "--staging", "build"][..], true),
            (&["build-eips", "--staging", "changed"][..], true),
            (
                &[
                    "build-eips",
                    "--staging",
                    "editorial",
                    "build",
                    "--against-upstream",
                ][..],
                true,
            ),
            (&["build-eips", "--production", "build"][..], false),
            (&["build-eips", "--production", "changed"][..], false),
        ] {
            assert_settings(
                arguments,
                &["ERCs"],
                None,
                ExecutionSettings {
                    build_root: None,
                    staging: expected_staging,
                    allow_dirty: false,
                    theme: SelectedSource::Remote,
                    sibling: SelectedSource::Remote,
                },
            );
        }
    }

    #[test]
    fn workspace_init_optional_flags_parse() {
        let template = parse_args(&[
            "build-eips",
            "workspace",
            "init",
            "/tmp/workspace",
            "--template",
        ]);
        let combined = parse_args(&[
            "build-eips",
            "workspace",
            "init",
            "/tmp/workspace",
            "--template",
            "--platform-dev",
        ]);

        assert!(matches!(
            template.operation,
            Operation::Workspace {
                command: WorkspaceCommand::Init {
                    template: true,
                    platform_dev: false,
                    ..
                }
            }
        ));
        assert!(matches!(
            combined.operation,
            Operation::Workspace {
                command: WorkspaceCommand::Init {
                    template: true,
                    platform_dev: true,
                    ..
                }
            }
        ));
    }

    #[test]
    fn explicit_workspace_config_path_is_not_accepted() {
        let error = Args::try_parse_from(["build-eips", "--config", "/tmp/config.toml", "build"])
            .unwrap_err();

        assert!(error.to_string().contains("unexpected argument '--config'"));
    }

    #[test]
    fn boolean_override_conflicts_are_hard_errors() {
        let args = parse_args(&["build-eips", "--staging", "--production", "build"]);
        let error = resolve_execution_settings(&args, &[], None).unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot pass both `--staging` and `--production`"));
    }

    #[test]
    fn local_first_without_workspace_config_requires_explicit_resolution() {
        let args = parse_args(&["build-eips", "build"]);
        let sibling_ids = vec!["ERCs".to_owned()];
        let error = resolve_execution_settings(&args, &sibling_ids, None).unwrap_err();
        let message = error.to_string();

        assert!(
            message.contains("selected command requires workspace-local theme and sibling sources")
        );
        assert!(message.contains("build-eips workspace init <workspace-root>"));
        assert!(message.contains("--remote-theme` and/or `--remote-sibling-repo"));
        assert!(message.contains(
            "use `parity <command>`, `--staging <command>`, or `--production <command>`"
        ));
        assert!(!message.contains("--profile"));
        assert!(!message.contains("--allow-dirty"));
        assert!(!message.contains("--theme <path>"));
        assert!(!message.contains("--sibling-repo <path>"));
    }

    #[test]
    fn local_first_with_remote_overrides_is_not_parity() {
        let local_args = parse_args(&[
            "build-eips",
            "--remote-theme",
            "--remote-sibling-repo",
            "build",
        ]);
        let sibling_ids = vec!["ERCs".to_owned()];
        let local_settings = resolve_execution_settings(&local_args, &sibling_ids, None).unwrap();

        let parity_args = parse_args(&["build-eips", "parity", "build"]);
        let parity_settings = resolve_execution_settings(&parity_args, &sibling_ids, None).unwrap();

        assert_eq!(
            local_settings,
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
    fn zero_sibling_remote_override_is_noop() {
        let remote_args = parse_args(&["build-eips", "--remote-sibling-repo", "parity", "build"]);
        let remote_settings = resolve_execution_settings(&remote_args, &[], None).unwrap();

        assert_eq!(remote_settings.sibling, SelectedSource::Remote);
    }

    #[test]
    fn zero_sibling_local_first_without_workspace_config_only_requires_theme_resolution() {
        let args = parse_args(&["build-eips", "build"]);
        let error = resolve_execution_settings(&args, &[], None).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("selected command requires workspace-local theme sources"));
        assert!(!message.contains("theme and sibling"));
        assert!(!message.contains("--theme <path>"));
        assert!(!message.contains("--sibling-repo <path>"));
    }

    #[test]
    fn manifest_identity_drives_runtime_resolution() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Core");
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &[]);
        let build_root = workspace.path().join("build-root");
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "--build-root",
            build_root.to_str().unwrap(),
            "parity",
            "build",
        ]);

        let resolved = resolve_execution(&args).unwrap();

        assert_eq!(resolved.repository_use.title, "Core");
        assert_eq!(resolved.repository_use.location.repository, active_url);
        assert!(resolved.repository_use.other_repos.is_empty());
        assert_eq!(resolved.build_path, build_root);
    }

    #[test]
    fn execution_commands_discover_workspace_config_from_active_repo_root() {
        let workspace = TempDir::new().unwrap();
        let workspace_root = workspace.path().join("workspace");
        let active_path = workspace_root.join("Core");
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &[]);
        std::fs::create_dir(workspace_root.join(config::DEFAULT_THEME_DIR)).unwrap();
        write_file(
            &workspace_root,
            config::LOCAL_CONFIG_FILE,
            &config::default_workspace_config_text(),
        );
        let args = parse_args(&["build-eips", "-C", active_path.to_str().unwrap(), "build"]);

        let resolved = resolve_execution(&args).unwrap();

        assert_eq!(
            resolved.build_path,
            workspace_root
                .join(config::DEFAULT_BUILD_ROOT_BASE)
                .join("Core")
        );
        assert_eq!(
            resolved.source_materialization,
            crate::git::SourceMaterialization::Dirty
        );
    }

    #[test]
    fn build_root_override_wins_with_workspace_config() {
        let workspace = TempDir::new().unwrap();
        let workspace_root = workspace.path().join("workspace");
        let active_path = workspace_root.join("Core");
        let active_url = file_url(&active_path);
        let build_root = workspace.path().join("override-build-root");
        write_manifest_repo(&active_path, "Core", &active_url, &[]);
        std::fs::create_dir(workspace_root.join(config::DEFAULT_THEME_DIR)).unwrap();
        write_file(
            &workspace_root,
            config::LOCAL_CONFIG_FILE,
            &config::default_workspace_config_text(),
        );
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "--build-root",
            build_root.to_str().unwrap(),
            "build",
        ]);

        let resolved = resolve_execution(&args).unwrap();

        assert_eq!(resolved.build_path, build_root);
    }

    #[test]
    fn non_workspace_build_path_falls_back_to_active_repo_build_dir() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Core");
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &[]);
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "parity",
            "build",
        ]);

        let resolved = resolve_execution(&args).unwrap();

        assert_eq!(resolved.build_path, active_path.join(super::BUILD_DIR));
    }

    #[test]
    fn unknown_repo_without_manifest_or_legacy_identity_errors() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Unknown");
        init_repo(&active_path, &[("content/0001.md", "# Proposal\n")]);
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "parity",
            "build",
        ]);

        let error = resolve_execution(&args).unwrap_err();
        let message = error.to_string();

        assert!(message.contains(config::REPO_MANIFEST_FILE));
        assert!(message.contains("legacy EIPs/ERCs identity fallback"));
    }

    #[test]
    fn malformed_repo_manifest_does_not_fall_back_to_legacy_identity() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Malformed");
        init_repo(&active_path, &[("content/0001.md", "# Proposal\n")]);
        write_file(&active_path, config::REPO_MANIFEST_FILE, "repo_id = [");
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "parity",
            "build",
        ]);

        let message = Report::from_error(resolve_execution(&args).unwrap_err()).to_string();

        assert!(message.contains("unable to load repo manifest"));
        assert!(!message.contains("legacy EIPs/ERCs identity fallback"));
    }

    #[test]
    fn workspace_local_sibling_mode_is_all_or_nothing() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Core");
        let eips_path = workspace.path().join("EIPs");
        init_repo(&eips_path, &[("content/0002.md", "# EIP\n")]);
        let siblings = vec![
            ("EIPs", file_url(&eips_path)),
            ("ERCs", file_url(&workspace.path().join("remotes/ERCs"))),
        ];
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &siblings);
        std::fs::write(
            workspace.path().join(config::LOCAL_CONFIG_FILE),
            config::default_workspace_config_text(),
        )
        .unwrap();
        let args = parse_args(&["build-eips", "-C", active_path.to_str().unwrap(), "build"]);

        let error = resolve_execution(&args).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("requires all declared sibling repos"));
        assert!(message.contains("ERCs"));
    }

    #[test]
    fn workspace_local_sources_resolve_from_standard_layout() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Core");
        let eips_path = workspace.path().join("EIPs");
        let ercs_path = workspace.path().join("ERCs");
        init_repo(&eips_path, &[("content/0002.md", "# EIP\n")]);
        init_repo(&ercs_path, &[("content/0003.md", "# ERC\n")]);
        std::fs::create_dir(workspace.path().join(config::DEFAULT_THEME_DIR)).unwrap();
        let siblings = vec![
            ("EIPs", file_url(&eips_path)),
            ("ERCs", file_url(&ercs_path)),
        ];
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &siblings);
        std::fs::write(
            workspace.path().join(config::LOCAL_CONFIG_FILE),
            config::default_workspace_config_text(),
        )
        .unwrap();
        let args = parse_args(&["build-eips", "-C", active_path.to_str().unwrap(), "build"]);

        let resolved = resolve_execution(&args).unwrap();

        match resolved.theme {
            ThemeSource::Local { path } => {
                assert_eq!(path, workspace.path().join(config::DEFAULT_THEME_DIR));
            }
            ThemeSource::Remote { .. } => panic!("expected workspace-local theme"),
        }
        assert_eq!(
            resolved.repository_use.other_repos["EIPs"],
            file_url(&eips_path)
        );
        assert_eq!(
            resolved.repository_use.other_repos["ERCs"],
            file_url(&ercs_path)
        );
    }

    fn init_workspace_source_repo(remotes_root: &Path, name: &str) -> Url {
        let path = remotes_root.join(name);
        init_repo(&path, &[("README.md", "workspace init test repo\n")]);
        file_url(&path)
    }

    fn workspace_init_test_repository_urls(remotes_root: &Path) -> (Url, Url, Url, Url) {
        (
            init_workspace_source_repo(remotes_root, "theme"),
            init_workspace_source_repo(remotes_root, "template"),
            init_workspace_source_repo(remotes_root, "preprocessor"),
            init_workspace_source_repo(remotes_root, "eipw"),
        )
    }

    fn assert_workspace_init_optional_repos(
        workspace_root: &Path,
        expect_template: bool,
        expect_platform_dev: bool,
    ) {
        assert!(Repository::open(workspace_root.join(config::DEFAULT_THEME_DIR)).is_ok());
        assert_eq!(
            Repository::open(workspace_root.join("template")).is_ok(),
            expect_template
        );
        assert_eq!(
            Repository::open(workspace_root.join("preprocessor")).is_ok(),
            expect_platform_dev
        );
        assert_eq!(
            Repository::open(workspace_root.join("eipw")).is_ok(),
            expect_platform_dev
        );
    }

    fn assert_workspace_init_and_doctor_for_siblings(sibling_ids: &[&str]) {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let remotes_root = temp.path().join("remotes");
        let (theme_url, template_url, preprocessor_url, eipw_url) =
            workspace_init_test_repository_urls(&remotes_root);
        let repositories = WorkspaceInitRepositories {
            theme: &theme_url,
            template: &template_url,
            preprocessor: &preprocessor_url,
            eipw: &eipw_url,
        };

        let sibling_repositories = sibling_ids
            .iter()
            .map(|sibling_id| {
                let sibling_id = *sibling_id;
                let sibling_path = remotes_root.join(sibling_id);
                let sibling_url = file_url(&sibling_path);
                write_manifest_repo(&sibling_path, sibling_id, &sibling_url, &[]);
                (sibling_id.to_owned(), sibling_url)
            })
            .collect::<Vec<_>>();
        let sibling_manifest_entries = sibling_repositories
            .iter()
            .map(|(repo_id, url)| (repo_id.as_str(), url.clone()))
            .collect::<Vec<_>>();
        let active_path = workspace_root.join("Core");
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &sibling_manifest_entries);
        let init_args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "workspace",
            "init",
            workspace_root.to_str().unwrap(),
        ]);

        init_workspace_with_repositories(
            &init_args,
            workspace_root.clone(),
            false,
            false,
            &repositories,
        )
        .unwrap();

        assert!(workspace_root.join(config::LOCAL_CONFIG_FILE).is_file());
        assert_workspace_init_optional_repos(&workspace_root, false, false);
        for sibling_id in sibling_ids {
            assert!(Repository::open(workspace_root.join(sibling_id)).is_ok());
        }

        let doctor_args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "workspace",
            "doctor",
        ]);
        let report = collect_doctor_report(&doctor_args, false).unwrap();

        assert_eq!(report.failures, 0);
    }

    fn assert_workspace_init_optional_clone_behavior(
        flags: &[&str],
        expect_template: bool,
        expect_platform_dev: bool,
    ) {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let remotes_root = temp.path().join("remotes");
        let (theme_url, template_url, preprocessor_url, eipw_url) =
            workspace_init_test_repository_urls(&remotes_root);
        let repositories = WorkspaceInitRepositories {
            theme: &theme_url,
            template: &template_url,
            preprocessor: &preprocessor_url,
            eipw: &eipw_url,
        };
        let active_path = workspace_root.join("Core");
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &[]);
        let active_path = active_path.to_string_lossy();
        let workspace_root_arg = workspace_root.to_string_lossy();
        let mut arguments = vec![
            "build-eips",
            "-C",
            active_path.as_ref(),
            "workspace",
            "init",
            workspace_root_arg.as_ref(),
        ];
        arguments.extend_from_slice(flags);
        let init_args = parse_args(&arguments);
        let Operation::Workspace {
            command:
                WorkspaceCommand::Init {
                    path,
                    template,
                    platform_dev,
                },
        } = init_args.operation.clone()
        else {
            panic!("expected workspace init command");
        };

        assert_eq!(template, expect_template);
        assert_eq!(platform_dev, expect_platform_dev);

        init_workspace_with_repositories(&init_args, path, template, platform_dev, &repositories)
            .unwrap();

        assert_workspace_init_optional_repos(&workspace_root, expect_template, expect_platform_dev);
    }

    #[test]
    fn workspace_init_and_doctor_cover_zero_one_and_many_siblings() {
        assert_workspace_init_and_doctor_for_siblings(&[]);
        assert_workspace_init_and_doctor_for_siblings(&["ERCs"]);
        assert_workspace_init_and_doctor_for_siblings(&["EIPs", "ERCs"]);
    }

    #[test]
    fn default_workspace_init_clones_required_repos_only() {
        assert_workspace_init_optional_clone_behavior(&[], false, false);
    }

    #[test]
    fn workspace_init_template_clones_template_only_as_optional_repo() {
        assert_workspace_init_optional_clone_behavior(&["--template"], true, false);
    }

    #[test]
    fn workspace_init_platform_dev_clones_platform_repos_only_as_optional_repos() {
        assert_workspace_init_optional_clone_behavior(&["--platform-dev"], false, true);
    }

    #[test]
    fn workspace_init_template_and_platform_dev_clone_all_optional_repos() {
        assert_workspace_init_optional_clone_behavior(
            &["--template", "--platform-dev"],
            true,
            true,
        );
    }

    #[test]
    fn workspace_doctor_missing_config_reports_one_failure_without_skip_warning() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Core");
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &[]);
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "workspace",
            "doctor",
        ]);

        let report = collect_doctor_report(&args, false).unwrap();

        assert_eq!(report.failures, 1);
        assert_eq!(report.warnings, 0);
    }

    #[test]
    fn workspace_doctor_parse_failed_config_reports_one_failure_without_skip_warning() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Core");
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &[]);
        std::fs::write(workspace.path().join(config::LOCAL_CONFIG_FILE), "[").unwrap();
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "workspace",
            "doctor",
        ]);

        let report = collect_doctor_report(&args, false).unwrap();

        assert_eq!(report.failures, 1);
        assert_eq!(report.warnings, 0);
    }

    #[test]
    fn workspace_doctor_removed_config_fields_report_parse_failure_check() {
        let workspace = TempDir::new().unwrap();
        let active_path = workspace.path().join("Core");
        let active_url = file_url(&active_path);
        write_manifest_repo(&active_path, "Core", &active_url, &[]);
        let config_path = workspace.path().join(config::LOCAL_CONFIG_FILE);
        std::fs::write(
            &config_path,
            r#"
build_root_base = ".local-build"
default_profile = "local"

[profiles.local]
staging = true
"#,
        )
        .unwrap();
        let error = LoadedWorkspaceConfig::from_path(&config_path).unwrap_err();
        assert!(matches!(error, config::WorkspaceError::Parse { .. }));
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "workspace",
            "doctor",
        ]);

        let report = collect_doctor_report(&args, false).unwrap();

        assert_eq!(report.failures, 1);
        assert_eq!(report.warnings, 0);
    }

    #[test]
    fn manifest_driven_multi_repo_build_and_editorial_flows_resolve_siblings() {
        let temp = TempDir::new().unwrap();
        let upstream_path = temp.path().join("upstream/Core");
        init_repo(
            &upstream_path,
            &[("content/0001.md", "# Original proposal\n")],
        );
        let upstream_url = file_url(&upstream_path);

        let active_path = temp.path().join("workspace/Core");
        std::fs::create_dir_all(active_path.parent().unwrap()).unwrap();
        git2::build::RepoBuilder::new()
            .clone(upstream_url.as_str(), &active_path)
            .unwrap();
        let active_repo = Repository::open(&active_path).unwrap();

        let eips_path = temp.path().join("remotes/EIPs");
        init_repo(&eips_path, &[("content/0002.md", "# EIP sibling\n")]);
        let ercs_path = temp.path().join("remotes/ERCs");
        init_repo(&ercs_path, &[("content/0003.md", "# ERC sibling\n")]);
        let siblings = vec![
            ("EIPs", file_url(&eips_path)),
            ("ERCs", file_url(&ercs_path)),
        ];
        write_repo_manifest_file(&active_path, "Core", &upstream_url, &siblings);
        append_and_commit(
            &active_repo,
            &active_path,
            &[("content/0001.md", "# Updated proposal\n")],
            "local proposal update",
        );
        let build_root = temp.path().join("build-root");
        let args = parse_args(&[
            "build-eips",
            "-C",
            active_path.to_str().unwrap(),
            "--build-root",
            build_root.to_str().unwrap(),
            "parity",
            "build",
        ]);
        let resolved = resolve_execution(&args).unwrap();

        assert_eq!(resolved.repository_use.other_repos.len(), 2);

        let repo_path = resolved.build_path.join(REPO_DIR);
        crate::git::Fresh::new(
            &resolved.root_path,
            &repo_path,
            resolved.repository_use.clone(),
            resolved.source_materialization,
        )
        .unwrap()
        .clone_src()
        .unwrap()
        .fetch_upstream()
        .unwrap()
        .merge()
        .unwrap();

        assert!(repo_path.join("content/0002.md").is_file());
        assert!(repo_path.join("content/0003.md").is_file());

        let selectors = EditorialSelectorArgs {
            paths: Vec::<PathBuf>::new(),
            batch: None,
            working_tree: false,
            against_upstream: true,
        };
        let targets = editorial_targets(&selectors, &resolved).unwrap();

        assert_eq!(targets, vec![PathBuf::from("content/0001.md")]);
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
