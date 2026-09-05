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
pub fn write_migration(
    journal: &mut Journal,
    dir: &Path,
    slug: &str,
    contents: &str,
) -> Result<Migration> {
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
    journal.write(&path, contents, false)?;
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

// ── Keeping a tree in step with a schema ────────────────────────────────

/// What writing one file did, or would have done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Nothing was there before.
    Created,
    /// Something was there and said something else.
    Updated,
    /// Something was there and already said this.
    Unchanged,
    /// Generated once, for a relation that is gone.
    Removed,
}

impl Change {
    /// Whether this counts as the tree having moved.
    pub const fn is_change(self) -> bool {
        !matches!(self, Self::Unchanged)
    }

    const fn verb(self, planned: bool) -> &'static str {
        match (self, planned) {
            (Self::Created, false) => "created",
            (Self::Created, true) => "would create",
            (Self::Updated, false) => "updated",
            (Self::Updated, true) => "would update",
            (Self::Unchanged, _) => "unchanged",
            (Self::Removed, false) => "removed",
            (Self::Removed, true) => "would remove",
        }
    }
}

/// One file's outcome.
#[derive(Debug)]
pub struct Entry {
    /// What happened to it.
    pub change: Change,
    /// The file in question.
    pub path: PathBuf,
    /// For a model that moved, which fields did.
    pub detail: Option<String>,
}

/// Every write a run makes, and what each one came to.
///
/// A run over an unchanged schema writes nothing and says so, which is
/// what makes regenerating safe to do on a habit rather than a decision.
/// In `check` mode nothing is written at all and the outcomes are what
/// *would* have happened — the shape CI wants, to ask whether a tree has
/// fallen behind its database.
#[derive(Debug)]
pub struct Journal {
    check: bool,
    entries: Vec<Entry>,
}

impl Journal {
    /// A journal that writes, or one that only reports.
    pub const fn new(check: bool) -> Self {
        Self {
            check,
            entries: Vec::new(),
        }
    }

    /// Whether this journal is only looking.
    pub const fn is_check(&self) -> bool {
        self.check
    }

    /// Write `contents` to `path` unless it already says exactly that.
    ///
    /// # Errors
    ///
    /// As [`write_file`]: a destination proto did not generate is left
    /// alone unless `force` is set.
    pub fn write(&mut self, path: &Path, contents: &str, force: bool) -> Result<Change> {
        let existing = match std::fs::read_to_string(path) {
            Ok(existing) => Some(existing),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(Error::ReadFile {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        let (change, detail) = match &existing {
            Some(old) if old == contents => (Change::Unchanged, None),
            Some(old) => (Change::Updated, field_changes(old, contents)),
            None => (Change::Created, None),
        };

        if change.is_change() && !self.check {
            write_file(path, contents, force)?;
        }
        self.entries.push(Entry {
            change,
            path: path.to_path_buf(),
            detail,
        });
        Ok(change)
    }

    /// Delete a file proto generated for something that no longer exists.
    ///
    /// A file without the generated marker is never removed, whatever it
    /// is called: proto only takes back what it put there.
    ///
    /// # Errors
    ///
    /// A read or delete failure on the file.
    pub fn remove(&mut self, path: &Path) -> Result<bool> {
        if !is_generated(path)? {
            return Ok(false);
        }
        if !self.check {
            std::fs::remove_file(path).map_err(|source| Error::WriteFile {
                path: path.to_path_buf(),
                source,
            })?;
        }
        self.entries.push(Entry {
            change: Change::Removed,
            path: path.to_path_buf(),
            detail: None,
        });
        Ok(true)
    }

    /// Whether anything moved, which is what `--check` exits on.
    pub fn changed(&self) -> bool {
        self.entries.iter().any(|e| e.change.is_change())
    }

    /// Whether this run touched files at all. A command that only reads
    /// has nothing to summarise.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// What happened, for stderr. Unchanged files are counted, not
    /// listed: a run that says nothing but "unchanged" is the point.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for entry in self.entries.iter().filter(|e| e.change.is_change()) {
            out.push_str(&format!(
                "  {} {}\n",
                entry.change.verb(self.check),
                entry.path.display()
            ));
            if let Some(detail) = &entry.detail {
                out.push_str(&format!("      {detail}\n"));
            }
        }

        let count = |change: Change| self.entries.iter().filter(|e| e.change == change).count();
        let tally = [
            (Change::Created, count(Change::Created)),
            (Change::Updated, count(Change::Updated)),
            (Change::Removed, count(Change::Removed)),
            (Change::Unchanged, count(Change::Unchanged)),
        ];
        let parts: Vec<String> = tally
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(change, n)| format!("{n} {}", change.verb(self.check)))
            .collect();

        out.push_str(&format!(
            "{}\n",
            if parts.is_empty() {
                "nothing to do".to_string()
            } else {
                parts.join(", ")
            }
        ));
        out
    }
}

/// Which fields differ between two generated structs.
///
/// Both sides are proto's own output, so the field lines have a known
/// shape and can be compared without parsing Rust. It is a summary for a
/// human reading a regeneration, not an analysis.
fn field_changes(old: &str, new: &str) -> Option<String> {
    let fields = |source: &str| -> Vec<(String, String)> {
        source
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix("pub ")?.strip_suffix(',')?;
                let (name, ty) = rest.split_once(": ")?;
                Some((name.to_string(), ty.to_string()))
            })
            .collect()
    };

    let (before, after) = (fields(old), fields(new));
    if before.is_empty() && after.is_empty() {
        return None;
    }

    let mut notes = Vec::new();
    for (name, ty) in &after {
        match before.iter().find(|(n, _)| n == name) {
            None => notes.push(format!("+{name}: {ty}")),
            Some((_, was)) if was != ty => notes.push(format!("~{name}: {was} -> {ty}")),
            Some(_) => {}
        }
    }
    for (name, _) in &before {
        if !after.iter().any(|(n, _)| n == name) {
            notes.push(format!("-{name}"));
        }
    }

    (!notes.is_empty()).then(|| notes.join(", "))
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    #[test]
    fn field_changes_read_as_a_column_diff() {
        let old =
            "pub struct X {\n    pub id: Uuid,\n    pub notes: String,\n    pub gone: i32,\n}";
        let new = "pub struct X {\n    pub id: Uuid,\n    pub notes: Option<String>,\n    pub added: bool,\n}";
        let changes = field_changes(old, new).unwrap();
        assert!(
            changes.contains("~notes: String -> Option<String>"),
            "{changes}"
        );
        assert!(changes.contains("+added: bool"), "{changes}");
        assert!(changes.contains("-gone"), "{changes}");
        assert!(!changes.contains("id"), "{changes}");
    }

    /// A directory of this run's own, so the tests do not tread on each
    /// other or on anything real.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iridium-proto-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_second_run_over_an_unchanged_file_writes_nothing() {
        let dir = scratch("idempotent");
        let path = dir.join("model.rs");
        let contents = "// @generated by proto 0.1.0\npub struct X {\n    pub id: i32,\n}\n";

        let mut journal = Journal::new(false);
        assert_eq!(
            journal.write(&path, contents, false).unwrap(),
            Change::Created
        );
        assert_eq!(
            journal.write(&path, contents, false).unwrap(),
            Change::Unchanged
        );
        assert!(journal.changed(), "the first write did change something");

        // A journal that only saw the second write has nothing to report.
        let mut quiet = Journal::new(false);
        assert_eq!(
            quiet.write(&path, contents, false).unwrap(),
            Change::Unchanged
        );
        assert!(!quiet.changed());
        // Counted, not listed, and not silent: "unchanged" is the
        // answer a regeneration is usually looking for.
        assert!(
            quiet.summary().contains("1 unchanged"),
            "{}",
            quiet.summary()
        );
        assert!(!quiet.summary().contains("model.rs"), "{}", quiet.summary());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_mode_reports_without_touching_anything() {
        let dir = scratch("check");
        let path = dir.join("model.rs");

        let mut journal = Journal::new(true);
        assert_eq!(
            journal.write(&path, "anything", false).unwrap(),
            Change::Created
        );
        assert!(!path.exists(), "check mode must not write");
        assert!(journal.changed());
        assert!(
            journal.summary().contains("would create"),
            "{}",
            journal.summary()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_takes_back_only_what_proto_wrote() {
        let dir = scratch("prune");
        let generated = dir.join("generated.rs");
        let handwritten = dir.join("handwritten.rs");
        std::fs::write(&generated, "// @generated by proto 0.1.0\npub struct X;\n").unwrap();
        std::fs::write(&handwritten, "pub fn helper() {}\n").unwrap();

        let mut journal = Journal::new(false);
        assert!(journal.remove(&generated).unwrap());
        assert!(!journal.remove(&handwritten).unwrap());
        assert!(!generated.exists());
        assert!(
            handwritten.exists(),
            "a file proto did not write must survive"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unchanged_struct_has_nothing_to_say() {
        let same = "pub struct X {\n    pub id: Uuid,\n}";
        assert!(field_changes(same, same).is_none());
    }
}
