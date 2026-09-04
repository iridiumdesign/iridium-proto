//! Where generated source goes. Default is stdout — that is what makes
//! `:%!proto model arboreal.species` work from inside Neovim. Writing to a
//! path refuses to clobber a file proto did not write.

use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::render::MARKER;

/// Write `contents` to `path`, creating parent directories. An existing file
/// without the generated marker is left alone unless `force` is set.
pub fn write_file(path: &Path, contents: &str, force: bool) -> Result<()> {
    if path.exists() && !force && !is_generated(path)? {
        return Err(Error::NotGenerated {
            path: path.to_path_buf(),
        });
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| Error::WriteFile {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, contents).map_err(|source| Error::WriteFile {
        path: path.to_path_buf(),
        source,
    })
}

fn is_generated(path: &Path) -> Result<bool> {
    let contents = std::fs::read_to_string(path).map_err(|source| Error::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(contents.lines().take(5).any(|line| line.contains(MARKER)))
}

pub fn write_stdout(contents: &str) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(contents.as_bytes())
        .and_then(|()| lock.flush())
        .map_err(|source| Error::WriteFile {
            path: "<stdout>".into(),
            source,
        })
}

pub fn warn(message: &str) {
    eprintln!("warning: {message}");
}
