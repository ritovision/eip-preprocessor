/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Theme source identity shared by runtime commands.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) enum ThemeSource {
    Remote { repository: String, commit: String },
    Local { path: PathBuf },
}
