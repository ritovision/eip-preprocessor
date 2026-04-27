/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

mod cache;
mod cli;
mod config;
mod context;
mod editorial;
mod execution;
mod find_root;
mod git;
mod github;
mod identity;
mod layout;
mod lint;
mod markdown;
mod preview;
mod print;
mod progress;
mod proposal;
mod serve;
mod theme;
mod workspace;
mod zola;

use std::path::{Path, PathBuf};

use clap::Parser;
use fslock::LockFile;
use log::{debug, info};
use snafu::{Report, ResultExt, Whatever};
use url::Url;

use crate::{
    cli::{Args, EditorialCommand, Operation, RuntimeOperation, WorkspaceCommand},
    config::ServerBinding,
    editorial::{editorial_runtime_execution, run_editorial_lint},
    execution::{resolve_execution, validate_non_execution_command_flags, ResolvedExecution},
    layout::{mounted_theme_path, output_path, CONTENT_DIR, REPO_DIR},
    proposal::is_proposal_path,
    serve::{serve_sync_config, DirtyServeWatcher, LocalThemeServeSync},
    theme::ThemeSource,
    workspace::{doctor_workspace, init_workspace},
};

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

fn make_build_dir(build_path: &Path) -> Result<PathBuf, Whatever> {
    if let Err(e) = std::fs::create_dir_all(build_path) {
        debug!(
            "got while creating build directory: {}",
            Report::from_error(e)
        );
    }
    Ok(build_path.to_path_buf())
}

fn prepare_theme_for_zola(
    theme: ThemeSource,
    repo_path: &Path,
) -> Result<(ThemeSource, Option<LocalThemeServeSync>), Whatever> {
    match theme {
        ThemeSource::Local { path } => {
            let mounted_theme_dir = mounted_theme_path(repo_path);
            git::materialize_working_tree(&path, &mounted_theme_dir)
                .whatever_context("unable to materialize workspace-local theme")?;
            let theme_index_path = git::index_path(&path)
                .whatever_context("unable to resolve workspace-local theme Git index path")?;

            Ok((
                ThemeSource::Local {
                    path: mounted_theme_dir.clone(),
                },
                Some(LocalThemeServeSync {
                    theme_source_root: path,
                    mounted_theme_dir,
                    theme_index_path,
                }),
            ))
        }
        ThemeSource::Remote { .. } => Ok((theme, None)),
    }
}

#[derive(Debug)]
struct Prepared {
    cache: cache::Cache,
    repo_path: PathBuf,
    output_path: PathBuf,
    repository_use: git::RepositoryUse,
    theme: ThemeSource,
    local_theme_sync: Option<LocalThemeServeSync>,
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
        let (theme, local_theme_sync) = prepare_theme_for_zola(theme, &repo_path)?;

        Ok(Prepared {
            repository_use,
            theme,
            local_theme_sync,
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
        let sync_config = serve_sync_config(
            self.source_materialization,
            &self.source_root,
            &self.repo_path,
            self.local_theme_sync.clone(),
        );
        let dirty_watcher = if sync_config.has_targets() {
            Some(
                DirtyServeWatcher::start(sync_config)
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
    use notify::{Event, EventKind};
    use snafu::Report;
    use tempfile::TempDir;
    use url::Url;

    use super::prepare_theme_for_zola;
    use crate::{
        cli::{
            Args, EditorialCommand, EditorialSelectorArgs, Operation, ProfiledOperation,
            RuntimeOperation, ServerCliArgs, WorkspaceCommand,
        },
        config::{self, LoadedWorkspaceConfig, ServerBinding},
        editorial::{editorial_runtime_execution, editorial_targets},
        execution::{
            explicit_environment_or_parity, resolve_base_url_override, resolve_execution,
            resolve_execution_settings, resolve_server_binding,
            validate_non_execution_command_flags, ExecutionSettings, ResolvedExecution,
            SelectedSource,
        },
        layout::{mounted_theme_path, theme_config_path, BUILD_DIR, REPO_DIR},
        serve::{event_has_theme_index_path, serve_sync_config, LocalThemeServeSync},
        theme::ThemeSource,
        workspace::{
            collect_doctor_report, init_workspace_with_repositories, WorkspaceInitRepositories,
        },
    };

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

    fn fake_theme_sync(root: &Path) -> LocalThemeServeSync {
        LocalThemeServeSync {
            theme_source_root: root.join("theme"),
            mounted_theme_dir: root.join("repo/themes/eips-theme"),
            theme_index_path: root.join("theme/.git/index"),
        }
    }

    #[test]
    fn local_theme_index_events_trigger_rescan() {
        let index_path = PathBuf::from("/workspace/theme/.git/index");
        let index_event = Event::new(EventKind::Any).add_path(index_path.clone());
        let lock_event =
            Event::new(EventKind::Any).add_path(PathBuf::from("/workspace/theme/.git/index.lock"));
        let unrelated_event =
            Event::new(EventKind::Any).add_path(PathBuf::from("/workspace/theme/.git/config"));

        assert!(event_has_theme_index_path(&index_path, &index_event));
        assert!(event_has_theme_index_path(&index_path, &lock_event));
        assert!(!event_has_theme_index_path(&index_path, &unrelated_event));
    }

    #[test]
    fn local_zola_runtime_commands_select_workspace_theme_for_materialization() {
        let workspace_config = load_workspace_config("");

        for arguments in [
            &["build-eips", "build"][..],
            &["build-eips", "check"][..],
            &["build-eips", "serve"][..],
            &["build-eips", "editorial", "build", "--against-upstream"][..],
        ] {
            let settings = settings_for(arguments, &["ERCs"], Some(&workspace_config));

            assert_eq!(settings.theme, SelectedSource::WorkspaceLocal);
        }
    }

    #[test]
    fn workspace_local_theme_is_materialized_as_mounted_theme_for_zola() {
        let temp = TempDir::new().unwrap();
        let theme_root = temp.path().join("workspace/theme");
        init_repo(
            &theme_root,
            &[
                ("config/zola.toml", "title = 'theme'\n"),
                ("templates/index.html", "local theme\n"),
            ],
        );
        let repo_path = temp.path().join("workspace/.local-build/Core/repo");

        let (theme, sync) = prepare_theme_for_zola(
            ThemeSource::Local {
                path: theme_root.clone(),
            },
            &repo_path,
        )
        .unwrap();

        let mounted_theme_dir = mounted_theme_path(&repo_path);
        assert!(matches!(theme, ThemeSource::Local { path } if path == mounted_theme_dir));
        assert_eq!(
            theme_config_path(&mounted_theme_dir),
            repo_path.join("themes/eips-theme/config/zola.toml")
        );
        assert_eq!(
            std::fs::read_to_string(mounted_theme_dir.join("templates/index.html")).unwrap(),
            "local theme\n"
        );
        let sync = sync.expect("local theme should enable serve sync");
        assert_eq!(sync.theme_source_root, theme_root);
        assert_eq!(sync.mounted_theme_dir, mounted_theme_dir);
        assert!(sync.theme_index_path.ends_with(".git/index"));
    }

    #[test]
    fn local_serve_syncs_theme_and_dirty_active_repo() {
        let temp = TempDir::new().unwrap();
        let workspace_config = load_workspace_config("");
        let settings = settings_for(&["build-eips", "serve"], &["ERCs"], Some(&workspace_config));
        let source_materialization = if settings.allow_dirty {
            crate::git::SourceMaterialization::Dirty
        } else {
            crate::git::SourceMaterialization::Clean
        };

        let sync_config = serve_sync_config(
            source_materialization,
            &temp.path().join("Core"),
            &temp.path().join(".local-build/Core/repo"),
            Some(fake_theme_sync(temp.path())),
        );

        assert!(sync_config.active_repo.is_some());
        assert!(sync_config.local_theme.is_some());
    }

    #[test]
    fn clean_local_serve_keeps_theme_sync_but_disables_active_repo_dirty_sync() {
        let temp = TempDir::new().unwrap();
        let workspace_config = load_workspace_config("");
        let settings = settings_for(
            &["build-eips", "serve", "--clean"],
            &["ERCs"],
            Some(&workspace_config),
        );
        let source_materialization = if settings.allow_dirty {
            crate::git::SourceMaterialization::Dirty
        } else {
            crate::git::SourceMaterialization::Clean
        };

        let sync_config = serve_sync_config(
            source_materialization,
            &temp.path().join("Core"),
            &temp.path().join(".local-build/Core/repo"),
            Some(fake_theme_sync(temp.path())),
        );

        assert!(sync_config.active_repo.is_none());
        assert!(sync_config.local_theme.is_some());
    }

    #[test]
    fn remote_and_environment_serve_paths_do_not_enable_local_theme_sync() {
        for arguments in [
            &["build-eips", "--remote-theme", "serve"][..],
            &["build-eips", "--staging", "serve"][..],
            &["build-eips", "--production", "serve"][..],
            &["build-eips", "parity", "serve"][..],
        ] {
            let settings = settings_for(arguments, &[], None);
            assert_eq!(settings.theme, SelectedSource::Remote);

            let (_theme, sync) = prepare_theme_for_zola(
                ThemeSource::Remote {
                    repository: "https://example.test/theme.git".to_owned(),
                    commit: "HEAD".to_owned(),
                },
                Path::new("/tmp/build/repo"),
            )
            .unwrap();

            assert!(sync.is_none());
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

        assert_eq!(resolved.build_path, active_path.join(BUILD_DIR));
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
