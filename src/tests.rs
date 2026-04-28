#![cfg(test)]

// Cross-domain behavior tests live here; see src/README.md for module test ownership.

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
    config::{self, LoadedWorkspaceConfig},
    editorial::editorial_targets,
    execution::{
        resolve_execution, resolve_execution_settings, validate_non_execution_command_flags,
        ExecutionSettings, SelectedSource,
    },
    layout::{BUILD_DIR, REPO_DIR},
    pipeline::prepare_theme_for_zola,
    theme::ThemeSource,
};

fn parse_args(arguments: &[&str]) -> Args {
    Args::try_parse_from(arguments).unwrap()
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
