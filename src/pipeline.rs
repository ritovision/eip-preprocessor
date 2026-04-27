/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Prepared Zola runtime pipeline.

use std::path::{Path, PathBuf};

use snafu::{ResultExt, Whatever};
use url::Url;

use crate::{
    cache,
    config::ServerBinding,
    execution::ResolvedExecution,
    git,
    layout::{mounted_theme_path, output_path, CONTENT_DIR, REPO_DIR},
    markdown,
    serve::{serve_sync_config, DirtyServeWatcher, LocalThemeServeSync},
    theme::ThemeSource,
    zola,
};

pub(crate) fn prepare_theme_for_zola(
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
pub(crate) struct Prepared {
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
    pub(crate) fn prepare(resolved: ResolvedExecution) -> Result<Self, Whatever> {
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

    pub(crate) fn build(self) -> Result<(), Whatever> {
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

    pub(crate) fn serve(self) -> Result<(), Whatever> {
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

    pub(crate) fn check(self) -> Result<(), Whatever> {
        zola::check(&self.theme, &self.cache, &self.repo_path)
            .whatever_context("zola check failed")?;
        Ok(())
    }
}
