/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Clap command surface and command helper methods.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use url::Url;

use crate::{lint, print};

/// Build script for Ethereum EIPs and ERCs.
#[derive(Parser, Debug)]
#[command(version, about)]
pub(crate) struct Args {
    /// Use ROOT as the base directory (instead of finding it automatically)
    #[clap(short = 'C')]
    pub(crate) root: Option<PathBuf>,

    /// Force the staging repositories and base URLs
    #[clap(long)]
    pub(crate) staging: bool,

    /// Force the production repositories and base URLs
    #[clap(long)]
    pub(crate) production: bool,

    /// Use the configured remote theme instead of a workspace-local theme
    #[clap(long)]
    pub(crate) remote_theme: bool,

    /// Use the configured remote sibling content repository
    #[clap(long)]
    pub(crate) remote_sibling_repo: bool,

    /// Write build artifacts under BUILD_ROOT instead of the default location
    #[clap(long)]
    pub(crate) build_root: Option<PathBuf>,

    #[clap(subcommand)]
    pub(crate) operation: Operation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub(crate) struct ServerCliArgs {
    /// Host/interface for the local server to bind
    #[arg(long)]
    pub(crate) host: Option<String>,

    /// Port for the local server to bind
    #[arg(long)]
    pub(crate) port: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub(crate) struct BaseUrlCliArgs {
    /// Override the rendered-site base URL for this command
    #[arg(long, value_parser = clap::value_parser!(Url))]
    pub(crate) base_url: Option<Url>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub(crate) struct CleanCliArgs {
    /// Ignore tracked working-tree changes in the active repo
    #[arg(long)]
    pub(crate) clean: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum Operation {
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
pub(crate) enum ProfiledOperation {
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
pub(crate) enum WorkspaceCommand {
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
pub(crate) enum EditorialCommand {
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
pub(crate) struct EditorialSelectorArgs {
    /// Repo-relative proposal path(s), such as `content/07949.md`
    #[arg(value_name = "PATH")]
    pub(crate) paths: Vec<PathBuf>,

    /// Read repo-relative proposal paths from BATCH, one per line
    #[arg(long)]
    pub(crate) batch: Option<PathBuf>,

    /// Select tracked dirty proposal files from the active content repo
    #[arg(long)]
    pub(crate) working_tree: bool,

    /// Select proposal files changed versus the upstream merge-base
    #[arg(long)]
    pub(crate) against_upstream: bool,
}

#[derive(Debug, clap::ValueEnum, Clone, Default)]
pub(crate) enum ChangedFormat {
    #[default]
    Newline,
    Nul,
    Json,
}

#[derive(Debug, Clone)]
pub(crate) enum RuntimeOperation {
    Build,
    Serve,
    Preview,
    Clean,
    Check,
    Changed { all: bool, format: ChangedFormat },
    Editorial { command: EditorialCommand },
}

impl Operation {
    pub(crate) fn server_cli_args(&self) -> ServerCliArgs {
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

    pub(crate) fn base_url_cli_args(&self) -> BaseUrlCliArgs {
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

    pub(crate) fn clean_cli_args(&self) -> CleanCliArgs {
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

    pub(crate) fn is_plain_site_command(&self) -> bool {
        matches!(
            self,
            Self::Build { .. } | Self::Serve { .. } | Self::Check { .. }
        )
    }

    pub(crate) fn is_editorial_build_command(&self) -> bool {
        matches!(
            self,
            Self::Editorial {
                command: EditorialCommand::Build { .. }
            }
        )
    }

    pub(crate) fn runtime_operation(&self) -> Option<RuntimeOperation> {
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

    pub(crate) fn is_workspace_command(&self) -> bool {
        matches!(self, Self::Workspace { .. })
    }

    pub(crate) fn is_print_command(&self) -> bool {
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

    pub(crate) fn print(&self, files: &[PathBuf], repo_path: &Path) {
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

impl EditorialSelectorArgs {
    pub(crate) fn selector_count(&self) -> usize {
        usize::from(!self.paths.is_empty())
            + usize::from(self.batch.is_some())
            + usize::from(self.working_tree)
            + usize::from(self.against_upstream)
    }
}
