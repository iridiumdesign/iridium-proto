//! Where generated source goes. Default is stdout — that is what makes
//! `:%!proto model shop.product` work from inside an editor. Writing to a
//! path refuses to clobber a file proto did not write.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::render::MARKER;

/// Write `contents` to `path`, creating parent directories as needed.
///
/// # Errors
///
/// [`Error::NotGenerated`] when the destination exists, lacks the
/// generated marker, and `force` is not set — overwriting it would
/// discard someone's work. Otherwise [`Error::ReadFile`] or
/// [`Error::WriteFile`] as the filesystem reports.
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

/// Write to stdout, which is where generated source goes by default.
///
/// # Errors
///
/// Fails if stdout does — a closed pipe, most often.
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

/// Warn on stderr, so a warning never lands in the generated source
/// being piped to stdout.
pub fn warn(message: &str) {
    eprintln!("warning: {message}");
}

// ── Migrations ──────────────────────────────────────────────────────────────

/// What [`write_migration`] did.
#[derive(Debug)]
pub enum Migration {
    /// A new file was written.
    Written(PathBuf),
    /// The newest migration for this table already says the same thing.
    /// Migrations are append-only and checksummed once applied, so the
    /// right move is to leave it alone.
    Unchanged(PathBuf),
}

/// Write a migration into `dir` as `YYYYMMDDNNN_<slug>.sql`, taking the
/// next free sequence for today.
///
/// # Errors
///
/// As [`write_file`], plus a read failure on the directory being
/// scanned for the sequence already in use.
///
/// An existing migration for the same slug whose body matches is left in
/// place: regenerating an unchanged table should not produce a second file
/// that says the same thing.
pub fn write_migration(dir: &Path, slug: &str, contents: &str) -> Result<Migration> {
    if let Some(existing) = newest_for(dir, slug)? {
        let previous = std::fs::read_to_string(&existing).map_err(|source| Error::ReadFile {
            path: existing.clone(),
            source,
        })?;
        if body(&previous) == body(contents) {
            return Ok(Migration::Unchanged(existing));
        }
    }

    let path = dir.join(format!("{}_{slug}.sql", next_version(dir)));
    write_file(&path, contents, false)?;
    Ok(Migration::Written(path))
}

/// The part of a migration that matters for comparison: everything from
/// the first statement on, leaving out the header with its date and
/// command line.
fn body(contents: &str) -> &str {
    contents
        .find("DO $$")
        .map_or(contents, |at| &contents[at..])
}

fn newest_for(dir: &Path, slug: &str) -> Result<Option<PathBuf>> {
    let suffix = format!("_{slug}.sql");
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::ReadFile {
                path: dir.to_path_buf(),
                source,
            });
        }
    };

    let mut newest: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(&suffix) && newest.as_ref().is_none_or(|n| path > *n) {
            newest = Some(path);
        }
    }
    Ok(newest)
}

/// `YYYYMMDDNNN`, taking the next sequence after the highest already used
/// today. Migrations from other days are ignored beyond ordering.
fn next_version(dir: &Path) -> String {
    let today = today();
    let mut highest = 0u32;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(version) = name.split('_').next() else {
                continue;
            };
            if version.len() == 11
                && version.starts_with(&today)
                && let Ok(seq) = version[8..].parse::<u32>()
            {
                highest = highest.max(seq);
            }
        }
    }

    format!("{today}{:03}", highest + 1)
}

/// Today as `YYYYMMDD`, in UTC.
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!("{year:04}{month:02}{day:02}")
}

/// Days since 1970-01-01 to a calendar date. Howard Hinnant's
/// `civil_from_days`, which is exact for any date this will ever see and
/// saves a date dependency for one line of output.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // Both land in 1..=31 and 1..=12, so the narrowing cannot lose
    // anything; `try_from` says that rather than asserting it.
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_700), (2026, 9, 4));
    }

    #[test]
    fn body_ignores_the_header() {
        let a = "-- @generated by proto 0.1.0\n-- regenerate: x\n\nDO $$\nSELECT 1;";
        let b = "-- @generated by proto 0.2.0\n-- regenerate: y\n\nDO $$\nSELECT 1;";
        assert_eq!(body(a), body(b));
    }
}
