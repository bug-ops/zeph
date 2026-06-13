// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

/// Write `data` to `path` atomically via a `.tmp` sibling and rename.
///
/// Creates a sibling file with a `.tmp` extension, writes all bytes to it, then
/// renames it over `path`. The rename is atomic on most filesystems, so readers
/// never observe a partial write.
///
/// # Errors
///
/// Returns [`std::io::Error`] if the write or rename fails.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
