use std::fs;
use std::io;
use std::path::Path;

/// One line of ani-cli's history file: `ep\tid\ttitle`. ani-cli writes the
/// file itself; ids are its provider's (e.g. hianime slugs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub ep: String,
    pub id: String,
    pub title: String,
}

/// Entries in file order; a missing file or malformed lines yield nothing.
pub fn read(path: &Path) -> Vec<HistoryEntry> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                eprintln!("ani-gui: reading {}: {err}", path.display());
            }
            return Vec::new();
        }
    };
    bytes
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            // The title keeps any further tabs, as ani-cli's `read -r ep id title` does.
            let mut parts = line.splitn(3, |&b| b == b'\t');
            let [ep, id, title] = [parts.next()?, parts.next()?, parts.next()?];
            if id.is_empty() {
                return None;
            }
            let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            Some(HistoryEntry {
                ep: text(ep),
                id: text(id),
                title: text(title),
            })
        })
        .collect()
}

/// Records `entry` as ani-cli's `update_history` would: replaces the line
/// for the same id, else appends one. For episodes found ahead of time with
/// [`crate::anicli::resolve_unrecorded`], once the user really plays them.
pub fn record(path: &Path, entry: &HistoryEntry) -> io::Result<()> {
    let old = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    let new_line = format!("{}\t{}\t{}", entry.ep, entry.id, entry.title);
    let mut lines: Vec<&[u8]> = old.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let same_id = |line: &[u8]| line.split(|&b| b == b'\t').nth(1) == Some(entry.id.as_bytes());
    let mut out = Vec::with_capacity(old.len() + new_line.len() + 1);
    let mut replaced = false;
    for line in lines {
        if !replaced && same_id(line) {
            out.extend_from_slice(new_line.as_bytes());
            replaced = true;
        } else {
            out.extend_from_slice(line);
        }
        out.push(b'\n');
    }
    if !replaced {
        out.extend_from_slice(new_line.as_bytes());
        out.push(b'\n');
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    // Not ani-cli's own `ani-hsts.new`, which a running ani-cli may be using.
    let tmp = dir.join(".ani-hsts.ani-gui");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ep: &str, id: &str, title: &str) -> HistoryEntry {
        HistoryEntry {
            ep: ep.into(),
            id: id.into(),
            title: title.into(),
        }
    }

    #[test]
    fn read_history_skips_malformed_lines() {
        let dir = std::env::temp_dir().join(format!("ani-gui-hist-{}", std::process::id()));
        let path = dir.join("ani-hsts");
        assert!(read(&path).is_empty());

        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &path,
            "5\tchiikawa-948\tChiikawa\n\
             \n\
             garbage without tabs\n\
             3\tfrieren-beyond-journeys-end-481\tFrieren: Beyond Journey's End\n\
             \t\t\n\
             7\takane-241\tAkane-banashi & \"Rakugo\" | 100% \\o/\n\
             2\ttabbed-1\tTitle\twith tab\n\
             1\ttrailing-1\tNo final newline",
        )
        .unwrap();
        assert_eq!(
            read(&path),
            [
                entry("5", "chiikawa-948", "Chiikawa"),
                entry(
                    "3",
                    "frieren-beyond-journeys-end-481",
                    "Frieren: Beyond Journey's End"
                ),
                entry("7", "akane-241", "Akane-banashi & \"Rakugo\" | 100% \\o/"),
                entry("2", "tabbed-1", "Title\twith tab"),
                entry("1", "trailing-1", "No final newline"),
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn record_replaces_the_shows_line_or_appends() {
        let dir = std::env::temp_dir().join(format!("ani-gui-hist-rec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("state/ani-cli/ani-hsts");

        // No file (or directory) yet: created with the one line.
        record(&path, &entry("1", "frieren-1", "Frieren")).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "1\tfrieren-1\tFrieren\n"
        );

        fs::write(
            &path,
            "5\tchiikawa-948\tChiikawa\n2\tfrieren-1\tFrieren\n7\tx\ty",
        )
        .unwrap();
        record(&path, &entry("3", "frieren-1", "Frieren")).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "5\tchiikawa-948\tChiikawa\n3\tfrieren-1\tFrieren\n7\tx\ty\n"
        );
        record(&path, &entry("1", "akane-241", "Akane-banashi")).unwrap();
        assert_eq!(
            read(&path).last(),
            Some(&entry("1", "akane-241", "Akane-banashi"))
        );
        assert_eq!(read(&path).len(), 4);
        fs::remove_dir_all(&dir).unwrap();
    }
}
