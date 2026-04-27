/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Proposal path classification helpers.

use std::{ffi::OsStr, path::Path};

use crate::layout::CONTENT_DIR;

pub(crate) fn is_proposal_path(path: &Path) -> bool {
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
