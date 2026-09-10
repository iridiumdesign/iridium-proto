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

/// The name every migration gets unless the config says otherwise: the
/// shape sqlx expects, an integer version, an underscore, a description.
pub const DEFAULT_MIGRATION_NAME: &str = "{version}_{slug}.sql";

/// The parts of one table that a migration's name can be built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationParts {
    /// The schema, as a Rust-safe identifier.
    pub schema: String,
    /// The table, likewise.
    pub table: String,
    /// `<schema>_<table>_crud`, which is what the default name uses.
    pub slug: String,
    /// Whatever `--migration-tag` said, for a pattern with `{tag}` in it.
    pub tag: Option<String>,
}

impl MigrationParts {
    /// For `schema.table`, with both already made into identifiers.
    #[must_use]
    pub fn new(schema: &str, table: &str) -> Self {
        Self {
            slug: format!("{schema}_{table}_crud"),
            schema: schema.to_string(),
            table: table.to_string(),
            tag: None,
        }
    }

    /// With a value for `{tag}`.
    #[must_use]
    pub fn with_tag(mut self, tag: Option<String>) -> Self {
        self.tag = tag;
        self
    }
}

/// Write a migration into `dir`, named by `name` — a `migration_name`
/// pattern such as [`DEFAULT_MIGRATION_NAME`] — taking the next free
/// sequence for today.
///
/// The pattern's placeholders are `{version}` (`YYYYMMDDNNN`), `{date}`
/// and `{seq}` (its two halves), `{schema}`, `{table}`, `{slug}`, and
/// `{tag}` for whatever `--migration-tag` said. Anything else in it is
/// written as given, which is where a project that stamps a target
/// version on its migrations puts it.
///
/// # Errors
///
/// [`Error::Usage`] for a pattern that cannot keep files or tables apart,
/// names a placeholder this does not know, or uses `{tag}` when no tag
/// was given. Otherwise as
/// [`write_file`], plus a read failure on the directory being scanned for
/// the sequence already in use.
///
/// An existing migration for the same table whose body matches is left in
/// place: regenerating an unchanged table should not produce a second file
/// that says the same thing.
pub fn write_migration(
    journal: &mut Journal,
    dir: &Path,
    name: &str,
    parts: &MigrationParts,
    contents: &str,
) -> Result<Migration> {
    let pattern = Pattern::parse(name)?;
    if pattern.0.contains(&Piece::Tag) && parts.tag.is_none() {
        return Err(Error::Usage(format!(
            "migration_name \"{name}\" uses {{tag}}, but no --migration-tag was given"
        )));
    }
    if let Some(existing) = newest_for(dir, &pattern, parts)? {
        let previous = std::fs::read_to_string(&existing).map_err(|source| Error::ReadFile {
            path: existing.clone(),
            source,
        })?;
        if body(&previous) == body(contents) {
            return Ok(Migration::Unchanged(existing));
        }
    }

    let today = today();
    let seq = next_seq(dir, &pattern, &today);
    let path = dir.join(pattern.render(&today, seq, parts));
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

/// One piece of a `migration_name` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    /// `{date}{seq}`, eleven digits.
    Version,
    /// `YYYYMMDD`.
    Date,
    /// `NNN`.
    Seq,
    Schema,
    Table,
    Slug,
    Tag,
}

impl Piece {
    /// How many digits a fixed-width piece takes; `None` for text and
    /// the names, which are as long as they are.
    fn digits(&self) -> Option<usize> {
        match self {
            Piece::Version => Some(11),
            Piece::Date => Some(8),
            Piece::Seq => Some(3),
            _ => None,
        }
    }
}

/// A `migration_name` taken apart, so it can be written and read back.
#[derive(Debug)]
struct Pattern(Vec<Piece>);

/// What a file name held in each placeholder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Captured<'n> {
    date: Option<&'n str>,
    seq: Option<&'n str>,
    schema: Option<&'n str>,
    table: Option<&'n str>,
    slug: Option<&'n str>,
    tag: Option<&'n str>,
}

impl<'n> Captured<'n> {
    /// Record `got` for `piece`. A placeholder used twice has to say the
    /// same thing both times.
    fn set(&mut self, piece: &Piece, got: &'n str) -> bool {
        let (slot, got) = match piece {
            Piece::Version => {
                let (date, seq) = got.split_at(8);
                return self.set(&Piece::Date, date) && self.set(&Piece::Seq, seq);
            }
            Piece::Date => (&mut self.date, got),
            Piece::Seq => (&mut self.seq, got),
            Piece::Schema => (&mut self.schema, got),
            Piece::Table => (&mut self.table, got),
            Piece::Slug => (&mut self.slug, got),
            Piece::Tag => (&mut self.tag, got),
            Piece::Text(_) => return true,
        };
        match slot {
            Some(already) => *already == got,
            None => {
                *slot = Some(got);
                true
            }
        }
    }

    fn seq(&self) -> u32 {
        self.seq.and_then(|s| s.parse().ok()).unwrap_or(0)
    }
}

impl Pattern {
    fn parse(pattern: &str) -> Result<Self> {
        let bad = |why: &str| Error::Usage(format!("invalid migration_name \"{pattern}\": {why}"));
        let mut pieces = Vec::new();
        let mut rest = pattern;
        while !rest.is_empty() {
            let Some(open) = rest.find('{') else {
                pieces.push(Piece::Text(rest.to_string()));
                break;
            };
            if open > 0 {
                pieces.push(Piece::Text(rest[..open].to_string()));
            }
            let Some(close) = rest[open..].find('}') else {
                return Err(bad("a '{' is never closed"));
            };
            pieces.push(match &rest[open + 1..open + close] {
                "version" => Piece::Version,
                "date" => Piece::Date,
                "seq" => Piece::Seq,
                "schema" => Piece::Schema,
                "table" => Piece::Table,
                "slug" => Piece::Slug,
                "tag" => Piece::Tag,
                other => return Err(bad(&format!("{{{other}}} is not a placeholder"))),
            });
            rest = &rest[open + close + 1..];
        }

        let has = |piece: Piece| pieces.contains(&piece);
        if !has(Piece::Version) && !has(Piece::Seq) {
            return Err(bad("it needs {version} or {seq} to keep files apart"));
        }
        if !has(Piece::Slug) && !has(Piece::Table) {
            return Err(bad("it needs {slug} or {table} to tell tables apart"));
        }
        Ok(Self(pieces))
    }

    fn render(&self, date: &str, seq: u32, parts: &MigrationParts) -> String {
        self.0
            .iter()
            .map(|piece| match piece {
                Piece::Text(t) => t.clone(),
                Piece::Version => format!("{date}{seq:03}"),
                Piece::Date => date.to_string(),
                Piece::Seq => format!("{seq:03}"),
                Piece::Schema => parts.schema.clone(),
                Piece::Table => parts.table.clone(),
                Piece::Slug => parts.slug.clone(),
                Piece::Tag => parts.tag.clone().unwrap_or_default(),
            })
            .collect()
    }

    /// Read the placeholders back out of a file name, if it fits.
    fn matches<'n>(&self, name: &'n str) -> Option<Captured<'n>> {
        let mut found = Captured::default();
        self.walk(0, name, &mut found).then_some(found)
    }

    fn walk<'n>(&self, at: usize, rest: &'n str, found: &mut Captured<'n>) -> bool {
        let Some(piece) = self.0.get(at) else {
            return rest.is_empty();
        };
        if let Piece::Text(text) = piece {
            return rest
                .strip_prefix(text.as_str())
                .is_some_and(|rest| self.walk(at + 1, rest, found));
        }
        if let Some(width) = piece.digits() {
            if !rest.is_char_boundary(width) || !rest[..width].bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            let (got, rest) = rest.split_at(width);
            let before = *found;
            if found.set(piece, got) && self.walk(at + 1, rest, found) {
                return true;
            }
            *found = before;
            return false;
        }
        // A name is as long as it is: try every non-empty prefix.
        for cut in rest
            .char_indices()
            .map(|(i, _)| i)
            .skip(1)
            .chain([rest.len()])
        {
            let (got, rest) = rest.split_at(cut);
            let before = *found;
            if found.set(piece, got) && self.walk(at + 1, rest, found) {
                return true;
            }
            *found = before;
        }
        false
    }

    /// Whether a name's captures are this table's migration. The tag is
    /// not part of that: a table whose CRUD has not changed keeps its
    /// migration across a tag bump.
    fn is_for(&self, found: &Captured<'_>, parts: &MigrationParts) -> bool {
        if self.0.contains(&Piece::Slug) {
            return found.slug == Some(parts.slug.as_str());
        }
        found.table == Some(parts.table.as_str())
            && (!self.0.contains(&Piece::Schema) || found.schema == Some(parts.schema.as_str()))
    }
}

/// Every file in `dir` that fits the pattern, with what its name says.
fn fitting(dir: &Path, pattern: &Pattern) -> Result<Vec<(PathBuf, String)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::ReadFile {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if pattern.matches(name).is_some() {
            files.push((path.clone(), name.to_string()));
        }
    }
    Ok(files)
}

fn newest_for(dir: &Path, pattern: &Pattern, parts: &MigrationParts) -> Result<Option<PathBuf>> {
    let mut newest: Option<(String, u32, PathBuf)> = None;
    for (path, name) in fitting(dir, pattern)? {
        let Some(found) = pattern.matches(&name) else {
            continue;
        };
        if !pattern.is_for(&found, parts) {
            continue;
        }
        let key = (found.date.unwrap_or("").to_string(), found.seq(), path);
        if newest.as_ref().is_none_or(|n| key > *n) {
            newest = Some(key);
        }
    }
    Ok(newest.map(|(_, _, path)| path))
}

/// The next sequence after the highest already used today by any file
/// that fits the pattern — another tool's migrations included, when the
/// name is loose enough to fit them. A pattern without a date counts
/// every file, so its sequence never restarts.
fn next_seq(dir: &Path, pattern: &Pattern, today: &str) -> u32 {
    let mut highest = 0u32;
    for (_, name) in fitting(dir, pattern).unwrap_or_default() {
        let Some(found) = pattern.matches(&name) else {
            continue;
        };
        if found.date.is_some_and(|d| d != today) {
            continue;
        }
        highest = highest.max(found.seq());
    }
    highest + 1
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
    /// Somebody took this file over — the marker is gone — so proto
    /// leaves it alone. Taking a file back is a supported thing to do,
    /// so this is reported rather than fatal.
    Skipped,
    /// The file no longer parses, so there is nothing to reconcile
    /// against. Replacing it would discard whatever is in it over what
    /// is probably a half-finished edit, so it is left alone.
    Unparsed,
}

impl Change {
    /// Whether this counts as the tree having moved. A file proto no
    /// longer manages has not moved; it is simply not proto's.
    pub const fn is_change(self) -> bool {
        !matches!(self, Self::Unchanged | Self::Skipped | Self::Unparsed)
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
            (Self::Skipped, _) => "left alone (not generated by proto)",
            (Self::Unparsed, _) => "left alone (does not parse)",
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

        // A Rust file that no longer parses cannot be reconciled, and
        // replacing it would throw away whatever is in it over what is
        // usually a half-finished edit. Leave it, say so, and let
        // whoever is mid-thought finish the thought.
        if !force
            && path.extension().is_some_and(|e| e == "rs")
            && let Some(old) = &existing
            && syn::parse_file(old).is_err()
        {
            self.entries.push(Entry {
                change: Change::Unparsed,
                path: path.to_path_buf(),
                detail: Some("fix the syntax error and run again".to_string()),
            });
            return Ok(Change::Unparsed);
        }

        // Where a file is already there, it is corrected rather than
        // replaced: only what disagrees with the database is edited, so
        // everything written around it stays where it was put.
        let contents = &match &existing {
            Some(old) => {
                crate::reconcile::reconcile(old, contents).unwrap_or_else(|| contents.to_string())
            }
            None => contents.to_string(),
        };

        let (change, detail) = match &existing {
            Some(old) if old == contents => (Change::Unchanged, None),
            // Somebody has taken this file over. Overwriting it would
            // discard their work, and failing would make every later
            // run fail too, so it is left where it is and reported.
            Some(old) if !force && !old.lines().take(5).any(|l| l.contains(MARKER)) => {
                (Change::Skipped, None)
            }
            Some(old) => {
                let notes = crate::inspect::differences(old, contents);
                let detail = (!notes.is_empty()).then(|| notes.join("; "));
                (Change::Updated, detail)
            }
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

    /// Record an outcome for a file corrected by other means than a
    /// rendered rewrite — the manifest, which is patched rather than
    /// reconciled. Nothing is written here: the caller has done that,
    /// or in check mode has not.
    pub fn record(&mut self, change: Change, path: &Path, detail: Option<String>) {
        self.entries.push(Entry {
            change,
            path: path.to_path_buf(),
            detail,
        });
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
        let listed = |c: Change| c.is_change() || matches!(c, Change::Skipped | Change::Unparsed);
        for entry in self.entries.iter().filter(|e| listed(e.change)) {
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
            (Change::Skipped, count(Change::Skipped)),
            (Change::Unparsed, count(Change::Unparsed)),
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

#[cfg(test)]
mod migration_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "iridium-proto-migration-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn product() -> MigrationParts {
        MigrationParts::new("shop", "product")
    }

    fn file_name(m: &Migration) -> String {
        let (Migration::Written(p) | Migration::Unchanged(p)) = m;
        p.file_name().unwrap().to_str().unwrap().to_string()
    }

    #[test]
    fn the_default_name_is_a_version_and_a_description() {
        let dir = scratch("default");
        let mut journal = Journal::new(false);
        let out = write_migration(
            &mut journal,
            &dir,
            DEFAULT_MIGRATION_NAME,
            &product(),
            "DO $$ 1",
        )
        .unwrap();
        let name = file_name(&out);
        assert_eq!(name, format!("{}001_shop_product_crud.sql", today()));
        assert!(matches!(out, Migration::Written(_)));
    }

    #[test]
    fn text_in_the_name_is_written_and_read_back() {
        let dir = scratch("literal");
        let mut journal = Journal::new(false);
        let name = "{version}_v1.4_{slug}.sql";
        let first = write_migration(&mut journal, &dir, name, &product(), "DO $$ 1").unwrap();
        assert_eq!(
            file_name(&first),
            format!("{}001_v1.4_shop_product_crud.sql", today())
        );

        // Same body: the file already there is the answer.
        let again = write_migration(&mut journal, &dir, name, &product(), "DO $$ 1").unwrap();
        assert!(matches!(again, Migration::Unchanged(_)), "{again:?}");
        assert_eq!(file_name(&again), file_name(&first));

        // A new body takes the next sequence.
        let next = write_migration(&mut journal, &dir, name, &product(), "DO $$ 2").unwrap();
        assert_eq!(
            file_name(&next),
            format!("{}002_v1.4_shop_product_crud.sql", today())
        );
    }

    #[test]
    fn the_sequence_steps_past_other_tools_migrations() {
        let dir = scratch("others");
        std::fs::write(dir.join(format!("{}005_add_users.sql", today())), "x").unwrap();
        std::fs::write(dir.join("20200101009_old_news.sql"), "x").unwrap();
        let mut journal = Journal::new(false);
        let out = write_migration(
            &mut journal,
            &dir,
            DEFAULT_MIGRATION_NAME,
            &product(),
            "DO $$ 1",
        )
        .unwrap();
        assert_eq!(
            file_name(&out),
            format!("{}006_shop_product_crud.sql", today())
        );
    }

    #[test]
    fn placeholders_come_back_out_of_a_name() {
        let pattern = Pattern::parse("{date}_{seq}_{schema}_{table}.sql").unwrap();
        let found = pattern.matches("20260908_003_shop_product.sql").unwrap();
        assert_eq!(found.date, Some("20260908"));
        assert_eq!(found.seq, Some("003"));
        assert_eq!(found.schema, Some("shop"));
        assert_eq!(found.table, Some("product"));
        assert!(pattern.is_for(&found, &product()));
        assert!(!pattern.is_for(&found, &MigrationParts::new("shop", "order")));

        let pattern = Pattern::parse("v2_{version}_{slug}.sql").unwrap();
        let found = pattern
            .matches("v2_20260908001_shop_product_crud.sql")
            .unwrap();
        assert_eq!(found.slug, Some("shop_product_crud"));
        assert!(
            pattern
                .matches("v3_20260908001_shop_product_crud.sql")
                .is_none()
        );
        assert!(
            pattern
                .matches("v2_2026090800_shop_product_crud.sql")
                .is_none()
        );
    }

    #[test]
    fn the_tag_is_written_and_is_not_part_of_which_table() {
        let dir = scratch("tag");
        let mut journal = Journal::new(false);
        let name = "{version}_{tag}_{slug}.sql";
        let tagged = |tag: &str| product().with_tag(Some(tag.to_string()));

        let first = write_migration(&mut journal, &dir, name, &tagged("v1.4"), "DO $$ 1").unwrap();
        assert_eq!(
            file_name(&first),
            format!("{}001_v1.4_shop_product_crud.sql", today())
        );

        // A new tag over the same CRUD is still the same migration.
        let bumped = write_migration(&mut journal, &dir, name, &tagged("v1.5"), "DO $$ 1").unwrap();
        assert!(matches!(bumped, Migration::Unchanged(_)), "{bumped:?}");

        // And new CRUD under the new tag carries it.
        let next = write_migration(&mut journal, &dir, name, &tagged("v1.5"), "DO $$ 2").unwrap();
        assert_eq!(
            file_name(&next),
            format!("{}002_v1.5_shop_product_crud.sql", today())
        );

        // A pattern that wants a tag has to be given one.
        let err = write_migration(&mut journal, &dir, name, &product(), "DO $$ 3").unwrap_err();
        assert!(err.to_string().contains("--migration-tag"), "{err}");
    }

    #[test]
    fn a_name_that_cannot_tell_files_apart_is_refused() {
        for bad in [
            "{slug}.sql",
            "{version}.sql",
            "{version}_{oops}.sql",
            "{version_{slug}.sql",
        ] {
            let err = Pattern::parse(bad).unwrap_err().to_string();
            assert!(err.contains("invalid migration_name"), "{bad}: {err}");
        }
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;

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

    /// Taking a file over is a supported thing to do, so it must not
    /// break every run after it.
    #[test]
    fn a_file_taken_over_is_left_alone_not_fatal() {
        let dir = scratch("owned");
        let path = dir.join("mine.rs");
        std::fs::write(&path, "// no marker here\npub struct Mine;\n").unwrap();

        let mut journal = Journal::new(false);
        assert_eq!(
            journal.write(&path, "regenerated", false).unwrap(),
            Change::Skipped
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "// no marker here\npub struct Mine;\n",
            "the file must be untouched"
        );
        // Not a change: a file proto no longer manages has not moved.
        assert!(!journal.changed());
        assert!(
            journal.summary().contains("left alone"),
            "{}",
            journal.summary()
        );

        // --force is still the way to take it back.
        let mut forced = Journal::new(false);
        assert_eq!(
            forced.write(&path, "regenerated", true).unwrap(),
            Change::Updated
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "regenerated");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A half-finished edit is not a reason to throw the file away.
    #[test]
    fn a_file_that_does_not_parse_is_left_alone() {
        let dir = scratch("unparsed");
        let path = dir.join("model.rs");
        let broken = "// @generated by proto 0.1.0\npub struct X {\n    pub fn oops() -> {\n";
        std::fs::write(&path, broken).unwrap();

        let mut journal = Journal::new(false);
        assert_eq!(
            journal.write(&path, "pub struct X {}\n", false).unwrap(),
            Change::Unparsed
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken, "untouched");
        // Not drift: the file is mid-edit, not out of step.
        assert!(!journal.changed());
        assert!(
            journal.summary().contains("does not parse"),
            "{}",
            journal.summary()
        );

        // --force is still the way through.
        let mut forced = Journal::new(false);
        assert_eq!(
            forced.write(&path, "pub struct X {}\n", true).unwrap(),
            Change::Updated
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
}
