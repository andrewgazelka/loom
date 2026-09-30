//! Where a file-backed store lives, for things that keep their own files beside it.
use crate::Store;
use std::path::{Path, PathBuf};

impl Store {
    /// The directory holding this store's database file, absolute when it can be
    /// resolved; `None` for an in-memory or temporary store, which has no file.
    pub fn directory(&self) -> Option<PathBuf> {
        let connection = self.connection.lock().ok()?;
        let file = Path::new(connection.path().filter(|path| !path.is_empty())?);
        let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        file.parent().map(Path::to_path_buf)
    }
}
