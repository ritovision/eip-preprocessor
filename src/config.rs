/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use snafu::{Backtrace, IntoError, OptionExt, ResultExt, Snafu};
use url::Url;

pub const LOCAL_CONFIG_FILE: &str = ".build-eips.toml";
pub const REPO_MANIFEST_FILE: &str = ".build-eips.repo.toml";
pub const DEFAULT_BUILD_ROOT_BASE: &str = ".local-build";
pub const DEFAULT_THEME_DIR: &str = "theme";
pub const LOCAL_PROFILE: &str = "local";
pub const PARITY_PROFILE: &str = "parity";
pub const DIRTY_PROFILE: &str = "dirty";
const RESERVED_WORKSPACE_NAMES: &[&str] = &[DEFAULT_THEME_DIR, "preprocessor", "eipw"];

#[derive(Debug, Snafu)]
pub enum RepoManifestError {
    #[snafu(display("i/o error while accessing `{}`", path.to_string_lossy()))]
    RepoFs {
        path: PathBuf,
        source: std::io::Error,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "unable to parse repo manifest `{}`",
        manifest_path.to_string_lossy()
    ))]
    RepoParse {
        manifest_path: PathBuf,
        #[snafu(source(from(toml::de::Error, Box::new)))]
        source: Box<toml::de::Error>,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "repo manifest `{}` is invalid: {reason}",
        manifest_path.to_string_lossy()
    ))]
    Invalid {
        manifest_path: PathBuf,
        reason: String,
        backtrace: Backtrace,
    },
}

#[derive(Debug, Snafu)]
pub enum WorkspaceError {
    #[snafu(display("i/o error while accessing `{}`", path.to_string_lossy()))]
    Fs {
        path: PathBuf,
        source: std::io::Error,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "unable to parse workspace config `{}`",
        config_path.to_string_lossy()
    ))]
    Parse {
        config_path: PathBuf,
        #[snafu(source(from(toml::de::Error, Box::new)))]
        source: Box<toml::de::Error>,
        backtrace: Backtrace,
    },

    #[snafu(display("cannot use `--profile {profile}` without a workspace config"))]
    ProfileWithoutConfig {
        profile: String,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "workspace config `{}` does not define profile `{profile}`",
        config_path.to_string_lossy()
    ))]
    MissingProfile {
        config_path: PathBuf,
        profile: String,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "workspace config `{}` defines reserved profile name `{profile}`",
        config_path.to_string_lossy()
    ))]
    ReservedProfileName {
        config_path: PathBuf,
        profile: String,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "profile name `{profile}` is reserved for a command group and cannot be selected"
    ))]
    ReservedProfileSelection {
        profile: String,
        backtrace: Backtrace,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    /// Where to fetch the theme from.
    pub repository: Url,

    /// Specific revision to checkout from the theme repository.
    pub commit: String,
}

/// Environment-specific repository metadata for an active proposal repo or sibling repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryEndpoint {
    /// Git repository to fetch proposal content from.
    pub repository: Url,

    /// Base URL where rendered HTML and assets for this repository are served.
    pub base_url: Url,
}

/// Tracked active-repo manifest loaded from `.build-eips.repo.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoManifest {
    /// Stable machine key for workspace directory names, build roots, and sibling references.
    pub repo_id: String,

    /// Production repository and base URL for this active repo.
    pub production: RepositoryEndpoint,

    /// Staging repository and base URL for this active repo.
    pub staging: RepositoryEndpoint,

    /// Directional sibling content repos used by this active repo.
    #[serde(default)]
    pub siblings: BTreeMap<String, RepoManifestSibling>,
}

impl RepoManifest {
    fn from_raw(raw: RawRepoManifest, manifest_path: &Path) -> Result<Self, RepoManifestError> {
        let repo_id = required_manifest_value(manifest_path, "repo_id", raw.repo_id)?;
        let production = required_manifest_value(manifest_path, "production", raw.production)?;
        let staging = required_manifest_value(manifest_path, "staging", raw.staging)?;
        let siblings = raw
            .siblings
            .into_iter()
            .map(|(repo_id, sibling)| {
                let production = required_manifest_value(
                    manifest_path,
                    &format!("siblings.{repo_id}.production"),
                    sibling.production,
                )?;
                let staging = required_manifest_value(
                    manifest_path,
                    &format!("siblings.{repo_id}.staging"),
                    sibling.staging,
                )?;

                Ok((
                    repo_id,
                    RepoManifestSibling {
                        production,
                        staging,
                    },
                ))
            })
            .collect::<Result<_, _>>()?;

        let manifest = Self {
            repo_id,
            production,
            staging,
            siblings,
        };
        manifest.validate(manifest_path)?;
        Ok(manifest)
    }

    fn validate(&self, manifest_path: &Path) -> Result<(), RepoManifestError> {
        validate_repo_key(manifest_path, "repo_id", &self.repo_id)?;

        if self.siblings.contains_key(&self.repo_id) {
            return InvalidSnafu {
                manifest_path: manifest_path.to_path_buf(),
                reason: format!(
                    "repo_id `{}` cannot also be declared as a sibling",
                    self.repo_id
                ),
            }
            .fail();
        }

        for sibling_id in self.siblings.keys() {
            validate_repo_key(manifest_path, "sibling key", sibling_id)?;
        }

        validate_unique_sibling_repositories(
            manifest_path,
            "production",
            self.siblings
                .iter()
                .map(|(id, sibling)| (id.as_str(), sibling.production.repository.as_str())),
        )?;
        validate_unique_sibling_repositories(
            manifest_path,
            "staging",
            self.siblings
                .iter()
                .map(|(id, sibling)| (id.as_str(), sibling.staging.repository.as_str())),
        )?;

        Ok(())
    }

    pub fn active_endpoint(&self, staging: bool) -> RepositoryEndpoint {
        if staging {
            self.staging.clone()
        } else {
            self.production.clone()
        }
    }

    pub fn sibling_repositories(&self, staging: bool) -> BTreeMap<String, Url> {
        self.siblings
            .iter()
            .map(|(repo_id, sibling)| {
                let endpoint = if staging {
                    &sibling.staging
                } else {
                    &sibling.production
                };
                (repo_id.clone(), endpoint.repository.clone())
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepoManifest {
    repo_id: Option<String>,
    production: Option<RepositoryEndpoint>,
    staging: Option<RepositoryEndpoint>,
    #[serde(default)]
    siblings: BTreeMap<String, RawRepoManifestSibling>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepoManifestSibling {
    production: Option<RepositoryEndpoint>,
    staging: Option<RepositoryEndpoint>,
}

/// Environment-specific metadata for one declared sibling content repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoManifestSibling {
    /// Production repository and base URL for this sibling repo.
    pub production: RepositoryEndpoint,

    /// Staging repository and base URL for this sibling repo.
    pub staging: RepositoryEndpoint,
}

#[derive(Debug, Clone)]
pub struct LoadedRepoManifest {
    manifest_path: PathBuf,
    manifest: RepoManifest,
}

impl LoadedRepoManifest {
    pub fn load(repo_root: &Path) -> Result<Option<Self>, RepoManifestError> {
        let manifest_path = repo_root.join(REPO_MANIFEST_FILE);
        match std::fs::read_to_string(&manifest_path) {
            Ok(contents) => Self::from_contents(manifest_path, &contents).map(Some),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(RepoFsSnafu {
                path: manifest_path,
            }
            .into_error(error)),
        }
    }

    #[cfg(test)]
    pub fn from_path(path: &Path) -> Result<Self, RepoManifestError> {
        let manifest_path = path.canonicalize().with_context(|_| RepoFsSnafu {
            path: path.to_path_buf(),
        })?;
        let contents = std::fs::read_to_string(&manifest_path).with_context(|_| RepoFsSnafu {
            path: manifest_path.clone(),
        })?;
        Self::from_contents(manifest_path, &contents)
    }

    fn from_contents(manifest_path: PathBuf, contents: &str) -> Result<Self, RepoManifestError> {
        let manifest =
            toml::from_str::<RawRepoManifest>(contents).with_context(|_| RepoParseSnafu {
                manifest_path: manifest_path.clone(),
            })?;
        let manifest = RepoManifest::from_raw(manifest, &manifest_path)?;

        Ok(Self {
            manifest_path,
            manifest,
        })
    }

    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    pub fn manifest(&self) -> &RepoManifest {
        &self.manifest
    }
}

fn required_manifest_value<T>(
    manifest_path: &Path,
    field: &str,
    value: Option<T>,
) -> Result<T, RepoManifestError> {
    value.with_context(|| InvalidSnafu {
        manifest_path: manifest_path.to_path_buf(),
        reason: format!("missing required `{field}` entry"),
    })
}

fn validate_repo_key(
    manifest_path: &Path,
    label: &str,
    key: &str,
) -> Result<(), RepoManifestError> {
    let invalid_reason = if key.is_empty() {
        Some("must not be empty")
    } else if matches!(key, "." | "..") {
        Some("must not be `.` or `..`")
    } else if key.contains('/') || key.contains('\\') {
        Some("must be a single safe path component")
    } else if RESERVED_WORKSPACE_NAMES.contains(&key) {
        Some("collides with a reserved workspace/platform directory name")
    } else {
        None
    };

    if let Some(reason) = invalid_reason {
        return InvalidSnafu {
            manifest_path: manifest_path.to_path_buf(),
            reason: format!("{label} `{key}` {reason}"),
        }
        .fail();
    }

    Ok(())
}

fn validate_unique_sibling_repositories<'a>(
    manifest_path: &Path,
    environment: &str,
    siblings: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<(), RepoManifestError> {
    let mut seen = HashSet::new();
    for (repo_id, repository) in siblings {
        if !seen.insert(repository) {
            return InvalidSnafu {
                manifest_path: manifest_path.to_path_buf(),
                reason: format!(
                    "duplicate {environment} sibling repository declaration `{repository}` under sibling key `{repo_id}`"
                ),
            }
            .fail();
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyLocation {
    /// Git repository to fetch proposals from.
    pub repository: Url,

    /// Location where the rendered HTML and assets will end up.
    pub base_url: Url,

    /// A commit hash that exists solely in this repository.
    ///
    /// Use to determine which repository is being rendered. Pick a commit after every other
    /// location/working group/etc. split off.
    pub identifying_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LegacyLocations(pub HashMap<String, LegacyLocation>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub theme: Theme,
    pub locations: LegacyLocations,
}

impl Config {
    pub fn production() -> Self {
        let mut locations = HashMap::new();

        locations.insert(
            "EIPs".into(),
            LegacyLocation {
                repository: "https://github.com/ethereum/EIPs.git".try_into().unwrap(),
                base_url: "https://eips.ethereum.org/".try_into().unwrap(),
                identifying_commit: "0f44e2b94df4e504bb7b912f56ebd712db2ad396".into(),
            },
        );

        locations.insert(
            "ERCs".into(),
            LegacyLocation {
                repository: "https://github.com/ethereum/ERCs.git".try_into().unwrap(),
                base_url: "https://ercs.ethereum.org/".try_into().unwrap(),
                identifying_commit: "8dd085d159cb123f545c272c0d871a5339550e79".into(),
            },
        );

        Self {
            theme: Theme {
                repository: "https://github.com/ethereum/eips-theme.git"
                    .try_into()
                    .unwrap(),
                commit: "0ddac35da36d311a8401c6cfb79c9991f78b647d".into(),
            },
            locations: LegacyLocations(locations),
        }
    }

    pub fn staging() -> Self {
        let mut locations = HashMap::new();

        locations.insert(
            "EIPs".into(),
            LegacyLocation {
                repository: "https://github.com/eips-wg/EIPs.git".try_into().unwrap(),
                base_url: "https://eips-wg.github.io/EIPs/".try_into().unwrap(),
                identifying_commit: "0f44e2b94df4e504bb7b912f56ebd712db2ad396".into(),
            },
        );

        locations.insert(
            "ERCs".into(),
            LegacyLocation {
                repository: "https://github.com/eips-wg/ERCs.git".try_into().unwrap(),
                base_url: "https://eips-wg.github.io/ERCs/".try_into().unwrap(),
                identifying_commit: "8dd085d159cb123f545c272c0d871a5339550e79".into(),
            },
        );

        Self {
            theme: Theme {
                repository: "https://github.com/eips-wg/theme.git".try_into().unwrap(),
                commit: "0ddac35da36d311a8401c6cfb79c9991f78b647d".into(),
            },
            locations: LegacyLocations(locations),
        }
    }
}

impl LegacyLocation {
    pub fn endpoint(&self) -> RepositoryEndpoint {
        RepositoryEndpoint {
            repository: self.repository.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

/// Selects whether a profile-backed source uses the workspace-local checkout or the remote
/// default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSelection {
    /// Use the remote/default source for this input.
    #[default]
    Remote,

    /// Use the workspace-local source for this input.
    Local,
}

/// Workspace-local configuration loaded from `.build-eips.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Profile name to use when no profile alias or `--profile` is passed.
    pub default_profile: Option<String>,

    /// Directory under the workspace root where local build artifacts are written.
    pub build_root_base: PathBuf,

    /// Custom profile definitions keyed by profile name.
    pub profiles: BTreeMap<String, LocalProfile>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            default_profile: None,
            build_root_base: DEFAULT_BUILD_ROOT_BASE.into(),
            profiles: BTreeMap::new(),
        }
    }
}

impl WorkspaceConfig {
    fn starter() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(LOCAL_PROFILE.into(), LocalProfile::local_default());

        Self {
            default_profile: Some(LOCAL_PROFILE.into()),
            build_root_base: DEFAULT_BUILD_ROOT_BASE.into(),
            profiles,
        }
    }

    fn validate(&self, config_path: &Path) -> Result<(), WorkspaceError> {
        for profile in self.profiles.keys() {
            if is_reserved_profile_name(profile) {
                return ReservedProfileNameSnafu {
                    config_path: config_path.to_path_buf(),
                    profile: profile.clone(),
                }
                .fail();
            }
        }

        Ok(())
    }
}

/// Profile defaults that participate in profile selection, `default_profile`, and CLI overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalProfile {
    /// Use the staging repositories and staging base URLs instead of production.
    pub staging: bool,

    /// Source preference for the theme input.
    pub theme: SourceSelection,

    /// Source preference for the sibling content repository input.
    pub sibling: SourceSelection,

    /// Enable dirty mode so tracked working-tree changes are materialized into the build input.
    pub allow_dirty: bool,
}

impl LocalProfile {
    fn local_default() -> Self {
        Self {
            staging: true,
            theme: SourceSelection::Local,
            sibling: SourceSelection::Local,
            allow_dirty: false,
        }
    }

    pub fn parity() -> Self {
        Self {
            staging: true,
            theme: SourceSelection::Remote,
            sibling: SourceSelection::Remote,
            allow_dirty: false,
        }
    }

    pub fn dirty() -> Self {
        Self {
            staging: true,
            theme: SourceSelection::Local,
            sibling: SourceSelection::Local,
            allow_dirty: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedWorkspaceConfig {
    config_path: PathBuf,
    workspace_root: PathBuf,
    config: WorkspaceConfig,
}

impl LoadedWorkspaceConfig {
    pub fn load(
        explicit: Option<&Path>,
        search_from: &Path,
    ) -> Result<Option<Self>, WorkspaceError> {
        match explicit {
            Some(path) => Self::from_path(path).map(Some),
            None => Self::discover(search_from),
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, WorkspaceError> {
        let config_path = path.canonicalize().with_context(|_| FsSnafu {
            path: path.to_path_buf(),
        })?;
        let contents = std::fs::read_to_string(&config_path).with_context(|_| FsSnafu {
            path: config_path.clone(),
        })?;
        let config = toml::from_str::<WorkspaceConfig>(&contents).with_context(|_| ParseSnafu {
            config_path: config_path.clone(),
        })?;
        config.validate(&config_path)?;

        let workspace_root = config_path
            .parent()
            .expect("workspace config should always have a parent")
            .to_path_buf();

        Ok(Self {
            config_path,
            workspace_root,
            config,
        })
    }

    pub fn discover(start: &Path) -> Result<Option<Self>, WorkspaceError> {
        match discover_path(start) {
            Some(path) => Self::from_path(&path).map(Some),
            None => Ok(None),
        }
    }

    pub fn selected_profile(
        &self,
        requested: Option<&str>,
    ) -> Result<Option<SelectedProfile>, WorkspaceError> {
        selected_profile(Some(self), requested)
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn build_root_for(&self, repo_name: &str) -> PathBuf {
        self.resolve_path(&self.config.build_root_base)
            .join(repo_name)
    }

    pub fn local_theme_path(&self) -> PathBuf {
        self.workspace_root.join(DEFAULT_THEME_DIR)
    }

    pub fn local_repo_path(&self, repo_name: &str) -> PathBuf {
        self.workspace_root.join(repo_name)
    }

    fn resolve_path(&self, path: &Path) -> PathBuf {
        self.workspace_root.join(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedProfile {
    pub name: String,
    pub profile: LocalProfile,
}

impl SelectedProfile {
    pub fn built_in(name: &str) -> Option<Self> {
        let profile = match name {
            PARITY_PROFILE => LocalProfile::parity(),
            DIRTY_PROFILE => LocalProfile::dirty(),
            _ => return None,
        };

        Some(Self {
            name: name.to_owned(),
            profile,
        })
    }
}

pub fn is_reserved_command_group_name(name: &str) -> bool {
    matches!(name, "editorial" | "workspace")
}

fn is_reserved_profile_name(name: &str) -> bool {
    matches!(
        name,
        PARITY_PROFILE | DIRTY_PROFILE | "editorial" | "workspace"
    )
}

fn select_profile(
    config: Option<&LoadedWorkspaceConfig>,
    name: &str,
) -> Result<SelectedProfile, WorkspaceError> {
    if is_reserved_command_group_name(name) {
        return ReservedProfileSelectionSnafu {
            profile: name.to_owned(),
        }
        .fail();
    }

    if let Some(profile) = SelectedProfile::built_in(name) {
        return Ok(profile);
    }

    let Some(config) = config else {
        return ProfileWithoutConfigSnafu {
            profile: name.to_owned(),
        }
        .fail();
    };

    let profile =
        config
            .config
            .profiles
            .get(name)
            .cloned()
            .with_context(|| MissingProfileSnafu {
                config_path: config.config_path.clone(),
                profile: name.to_owned(),
            })?;

    Ok(SelectedProfile {
        name: name.to_owned(),
        profile,
    })
}

pub fn selected_profile(
    config: Option<&LoadedWorkspaceConfig>,
    requested: Option<&str>,
) -> Result<Option<SelectedProfile>, WorkspaceError> {
    let name = requested.or_else(|| {
        config.and_then(|loaded_config| loaded_config.config.default_profile.as_deref())
    });

    match name {
        Some(name) => select_profile(config, name).map(Some),
        None => Ok(None),
    }
}

pub fn discover_path(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);

    while let Some(candidate) = current {
        let config_path = candidate.join(LOCAL_CONFIG_FILE);
        match std::fs::File::open(&config_path) {
            Ok(_) => return Some(config_path),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                current = candidate.parent();
            }
            Err(_) => return Some(config_path),
        }
    }

    None
}

pub fn default_workspace_config_text() -> String {
    toml::to_string_pretty(&WorkspaceConfig::starter())
        .expect("workspace starter config should serialize")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{
        default_workspace_config_text, discover_path, selected_profile, LoadedRepoManifest,
        LoadedWorkspaceConfig, LocalProfile, RepoManifestError, SourceSelection, WorkspaceError,
        DIRTY_PROFILE, LOCAL_CONFIG_FILE, LOCAL_PROFILE, PARITY_PROFILE, REPO_MANIFEST_FILE,
    };

    struct TestWorkspace {
        tempdir: TempDir,
    }

    impl TestWorkspace {
        fn new() -> Self {
            Self {
                tempdir: TempDir::new().unwrap(),
            }
        }

        fn root(&self) -> &Path {
            self.tempdir.path()
        }

        fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
            self.root().join(relative)
        }

        fn write_file(&self, relative: impl AsRef<Path>, contents: &str) -> PathBuf {
            let path = self.path(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
            path
        }

        fn create_dir(&self, relative: impl AsRef<Path>) -> PathBuf {
            let path = self.path(relative);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    fn manifest_text(repo_id: &str, siblings: &str) -> String {
        format!(
            r#"
repo_id = "{repo_id}"

[production]
repository = "https://example.test/{repo_id}.git"
base_url = "https://example.test/{repo_id}/"

[staging]
repository = "https://staging.example.test/{repo_id}.git"
base_url = "https://staging.example.test/{repo_id}/"

{siblings}
"#
        )
    }

    fn manifest_invalid_reason(error: RepoManifestError) -> String {
        match error {
            RepoManifestError::Invalid { reason, .. } => reason,
            other => panic!("expected invalid repo manifest, got {other:?}"),
        }
    }

    #[test]
    fn missing_repo_manifest_loads_as_none() {
        let workspace = TestWorkspace::new();

        assert!(LoadedRepoManifest::load(workspace.root())
            .unwrap()
            .is_none());
    }

    #[test]
    fn parses_repo_manifest_with_directional_siblings() {
        let workspace = TestWorkspace::new();
        let manifest_path = workspace.write_file(
            REPO_MANIFEST_FILE,
            &manifest_text(
                "Core",
                r#"
[siblings.EIPs.production]
repository = "https://example.test/EIPs.git"
base_url = "https://example.test/EIPs/"

[siblings.EIPs.staging]
repository = "https://staging.example.test/EIPs.git"
base_url = "https://staging.example.test/EIPs/"
"#,
            ),
        );

        let manifest = LoadedRepoManifest::from_path(&manifest_path).unwrap();

        assert_eq!(manifest.manifest().repo_id, "Core");
        assert_eq!(manifest.manifest().siblings.len(), 1);
        assert!(manifest.manifest().siblings.contains_key("EIPs"));
    }

    #[test]
    fn repo_manifest_requires_identity_and_environments() {
        let workspace = TestWorkspace::new();
        let manifest_path = workspace.write_file(
            REPO_MANIFEST_FILE,
            r#"
[production]
repository = "https://example.test/Core.git"
base_url = "https://example.test/Core/"
"#,
        );

        let reason =
            manifest_invalid_reason(LoadedRepoManifest::from_path(&manifest_path).unwrap_err());

        assert!(reason.contains("missing required `repo_id` entry"));

        let manifest_path = workspace.write_file(
            REPO_MANIFEST_FILE,
            r#"
repo_id = "Core"

[production]
repository = "https://example.test/Core.git"
base_url = "https://example.test/Core/"
"#,
        );
        let reason =
            manifest_invalid_reason(LoadedRepoManifest::from_path(&manifest_path).unwrap_err());

        assert!(reason.contains("missing required `staging` entry"));

        let manifest_path = workspace.write_file(
            REPO_MANIFEST_FILE,
            r#"
repo_id = "Core"

[staging]
repository = "https://staging.example.test/Core.git"
base_url = "https://staging.example.test/Core/"
"#,
        );
        let reason =
            manifest_invalid_reason(LoadedRepoManifest::from_path(&manifest_path).unwrap_err());

        assert!(reason.contains("missing required `production` entry"));
    }

    #[test]
    fn repo_manifest_rejects_unsafe_and_reserved_keys() {
        let workspace = TestWorkspace::new();
        let manifest_path = workspace.write_file(REPO_MANIFEST_FILE, &manifest_text("theme", ""));

        let reason =
            manifest_invalid_reason(LoadedRepoManifest::from_path(&manifest_path).unwrap_err());

        assert!(reason.contains("repo_id `theme`"));
        assert!(reason.contains("reserved"));

        let manifest_path =
            workspace.write_file(REPO_MANIFEST_FILE, &manifest_text("Core/Meta", ""));
        let reason =
            manifest_invalid_reason(LoadedRepoManifest::from_path(&manifest_path).unwrap_err());

        assert!(reason.contains("repo_id `Core/Meta`"));
        assert!(reason.contains("single safe path component"));
    }

    #[test]
    fn repo_manifest_rejects_self_sibling() {
        let workspace = TestWorkspace::new();
        let manifest_path = workspace.write_file(
            REPO_MANIFEST_FILE,
            &manifest_text(
                "Core",
                r#"
[siblings.Core.production]
repository = "https://example.test/Core.git"
base_url = "https://example.test/Core/"

[siblings.Core.staging]
repository = "https://staging.example.test/Core.git"
base_url = "https://staging.example.test/Core/"
"#,
            ),
        );

        let reason =
            manifest_invalid_reason(LoadedRepoManifest::from_path(&manifest_path).unwrap_err());

        assert!(reason.contains("cannot also be declared as a sibling"));
    }

    #[test]
    fn repo_manifest_rejects_duplicate_sibling_repositories_per_environment() {
        let workspace = TestWorkspace::new();
        let manifest_path = workspace.write_file(
            REPO_MANIFEST_FILE,
            &manifest_text(
                "Core",
                r#"
[siblings.EIPs.production]
repository = "https://example.test/shared.git"
base_url = "https://example.test/EIPs/"

[siblings.EIPs.staging]
repository = "https://staging.example.test/EIPs.git"
base_url = "https://staging.example.test/EIPs/"

[siblings.ERCs.production]
repository = "https://example.test/shared.git"
base_url = "https://example.test/ERCs/"

[siblings.ERCs.staging]
repository = "https://staging.example.test/ERCs.git"
base_url = "https://staging.example.test/ERCs/"
"#,
            ),
        );

        let reason =
            manifest_invalid_reason(LoadedRepoManifest::from_path(&manifest_path).unwrap_err());

        assert!(reason.contains("duplicate production sibling repository declaration"));
    }

    #[test]
    fn parses_default_workspace_config() {
        let workspace = TestWorkspace::new();
        let config_path = workspace.write_file(LOCAL_CONFIG_FILE, &default_workspace_config_text());

        let config = LoadedWorkspaceConfig::from_path(&config_path).unwrap();
        let local = config
            .selected_profile(Some(LOCAL_PROFILE))
            .unwrap()
            .unwrap()
            .profile;

        assert_eq!(
            local,
            LocalProfile {
                staging: true,
                theme: SourceSelection::Local,
                sibling: SourceSelection::Local,
                allow_dirty: false,
            }
        );
        assert!(config
            .selected_profile(Some(PARITY_PROFILE))
            .unwrap()
            .is_some());
        assert!(config
            .selected_profile(Some(DIRTY_PROFILE))
            .unwrap()
            .is_some());
    }

    #[test]
    fn starter_workspace_config_roundtrips_stably() {
        let original = default_workspace_config_text();
        let parsed = toml::from_str::<super::WorkspaceConfig>(&original).unwrap();
        let reparsed = toml::to_string_pretty(&parsed).unwrap();

        assert_eq!(reparsed, original);
        assert!(!original.contains("[profiles.parity]"));
        assert!(!original.contains("[profiles.dirty]"));
        assert!(original.contains("default_profile = \"local\""));
        assert!(original.contains("[profiles.local]"));
    }

    #[test]
    fn built_in_profiles_work_without_workspace_config() {
        let parity = selected_profile(None, Some(PARITY_PROFILE))
            .unwrap()
            .unwrap();
        let dirty = selected_profile(None, Some(DIRTY_PROFILE))
            .unwrap()
            .unwrap();

        assert_eq!(parity.profile, LocalProfile::parity());
        assert_eq!(dirty.profile, LocalProfile::dirty());
    }

    #[test]
    fn custom_profile_without_workspace_config_fails() {
        let error = selected_profile(None, Some(LOCAL_PROFILE)).unwrap_err();

        assert!(matches!(error, WorkspaceError::ProfileWithoutConfig { .. }));
    }

    #[test]
    fn reserved_command_group_name_cannot_be_selected_as_profile() {
        let error = selected_profile(None, Some("editorial")).unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::ReservedProfileSelection { .. }
        ));
    }

    #[test]
    fn config_defined_profile_names_cannot_collide_with_reserved_names() {
        let workspace = TestWorkspace::new();
        let config_path = workspace.write_file(
            LOCAL_CONFIG_FILE,
            r#"
[profiles.parity]
staging = true
"#,
        );

        let error = LoadedWorkspaceConfig::from_path(&config_path).unwrap_err();

        assert!(matches!(error, WorkspaceError::ReservedProfileName { .. }));
    }

    #[test]
    fn old_inverse_schema_fails_to_parse() {
        let workspace = TestWorkspace::new();
        let config_path = workspace.write_file(
            LOCAL_CONFIG_FILE,
            r#"
[profiles.local]
staging = true
use_local_theme = true
use_local_sibling = true
"#,
        );

        let error = LoadedWorkspaceConfig::from_path(&config_path).unwrap_err();

        assert!(matches!(error, WorkspaceError::Parse { .. }));
    }

    #[test]
    fn default_profile_resolves_to_built_in_profile() {
        let workspace = TestWorkspace::new();
        let config_path = workspace.write_file(
            LOCAL_CONFIG_FILE,
            r#"
default_profile = "parity"
"#,
        );

        let config = LoadedWorkspaceConfig::from_path(&config_path).unwrap();
        let selected = config.selected_profile(None).unwrap().unwrap();

        assert_eq!(selected.name, PARITY_PROFILE);
        assert_eq!(selected.profile, LocalProfile::parity());
    }

    #[test]
    fn default_profile_resolves_to_custom_profile() {
        let workspace = TestWorkspace::new();
        let config_path = workspace.write_file(
            LOCAL_CONFIG_FILE,
            r#"
default_profile = "custom"

[profiles.custom]
staging = true
theme = "local"
sibling = "remote"
"#,
        );

        let config = LoadedWorkspaceConfig::from_path(&config_path).unwrap();
        let selected = config.selected_profile(None).unwrap().unwrap();

        assert_eq!(selected.name, "custom");
        assert_eq!(
            selected.profile,
            LocalProfile {
                staging: true,
                theme: SourceSelection::Local,
                sibling: SourceSelection::Remote,
                allow_dirty: false,
            }
        );
    }

    #[test]
    fn discover_path_walks_upward() {
        let workspace = TestWorkspace::new();
        let config_path = workspace.write_file(LOCAL_CONFIG_FILE, &default_workspace_config_text());
        let nested = workspace.create_dir("EIPs/content");

        assert_eq!(discover_path(&nested).unwrap(), config_path);
        assert_eq!(
            LoadedWorkspaceConfig::discover(&nested)
                .unwrap()
                .unwrap()
                .config_path(),
            config_path
        );
    }

    #[test]
    fn missing_workspace_config_is_not_discovered() {
        let workspace = TestWorkspace::new();
        let nested = workspace.create_dir("EIPs/content");

        assert!(discover_path(&nested).is_none());
        assert!(LoadedWorkspaceConfig::discover(&nested).unwrap().is_none());
    }
}
