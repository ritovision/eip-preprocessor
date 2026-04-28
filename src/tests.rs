#![cfg(test)]

use std::path::{Path, PathBuf};

use clap::Parser;
use git2::{IndexAddOption, Repository, Signature};
use snafu::Report;
use tempfile::TempDir;
use url::Url;

use crate::{
    cli::{
        Args, EditorialCommand, EditorialSelectorArgs, Operation, RuntimeOperation,
        WorkspaceCommand,
    },
    config::{self, LoadedWorkspaceConfig, ServerBinding},
    editorial::{editorial_runtime_execution, editorial_targets},
    execution::{
        resolve_execution, resolve_execution_settings, validate_non_execution_command_flags,
        ExecutionSettings, ResolvedExecution, SelectedSource,
    },
    layout::{mounted_theme_path, theme_config_path, BUILD_DIR, REPO_DIR},
    pipeline::prepare_theme_for_zola,
    serve::{serve_sync_config, LocalThemeServeSync},
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

fn write_repo_manifest_file(path: &Path, repo_id: &str, upstream: &Url, siblings: &[(&str, Url)]) {
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

fn fake_theme_sync(root: &Path) -> LocalThemeServeSync {
    LocalThemeServeSync {
        theme_source_root: root.join("theme"),
        mounted_theme_dir: root.join("repo/themes/eips-theme"),
        theme_index_path: root.join("theme/.git/index"),
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
    assert_workspace_init_optional_clone_behavior(&["--template", "--platform-dev"], true, true);
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
