//! Wrap a nix file and manage corresponding state.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use NixFile;

/// A “project” knows how to handle the lorri state
/// for a given nix file.
#[derive(Debug, Clone)]
pub struct Project {
    /// Absolute path to this project’s nix file.
    pub nix_file: NixFile,

    /// Directory in which this project’s
    /// garbage collection roots are stored.
    pub gc_root_path: PathBuf,

    /// Hash of the nix file’s absolute path.
    hash: String,
}

impl Project {
    /// Construct a `Project` from nix file path
    /// and the base GC root directory
    /// (as returned by `Paths.gc_root_dir()`),
    pub fn new(nix_file: NixFile, gc_root_dir: &Path) -> std::io::Result<Project> {
        let hash = format!("{:x}", md5::compute(nix_file.as_os_str().as_bytes()));
        let project_gc_root = gc_root_dir.join(&hash).join("gc_root").to_path_buf();

        std::fs::create_dir_all(&project_gc_root)?;

        Ok(Project {
            nix_file,
            gc_root_path: project_gc_root,
            hash,
        })
    }

    /// Generate a "unique" ID for this project based on its absolute path.
    pub fn hash(&self) -> &str {
        &self.hash
    }
}
