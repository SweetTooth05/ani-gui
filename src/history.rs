use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// One line of ani-cli's history file: `ep\tid\ttitle`.
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
            let [ep, id, title] = fields(line)?;
            let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            Some(HistoryEntry {
                ep: text(ep),
                id: text(id),
                title: text(title),
            })
        })
        .collect()
}

/// Replaces the line(s) for `entry.id`, or appends one, leaving every other
/// line byte-for-byte as it was. The file is replaced atomically.
pub fn record(path: &Path, entry: &HistoryEntry) -> io::Result<()> {
    let invalid = |s: &str| s.is_empty() || s.contains(['\t', '\n', '\r']);
    if invalid(&entry.id) || invalid(&entry.ep) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid history entry {:?} / {:?}", entry.ep, entry.id),
        ));
    }
    let title = entry.title.replace(['\t', '\n', '\r'], " ");
    let new_line = format!("{}\t{}\t{title}", entry.ep, entry.id).into_bytes();

    let existing = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    let mut lines: Vec<&[u8]> = existing.split(|&b| b == b'\n').collect();
    if existing.is_empty() || existing.ends_with(b"\n") {
        lines.pop();
    }
    let mut replaced = false;
    for line in &mut lines {
        if fields(line).is_some_and(|[_, id, _]| id == entry.id.as_bytes()) {
            *line = &new_line;
            replaced = true;
        }
    }
    if !replaced {
        lines.push(&new_line);
    }

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp_name = path.file_name().map(OsString::from).unwrap_or_default();
    tmp_name.push(".ani-gui.tmp");
    let tmp = path.with_file_name(tmp_name);
    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        for line in &lines {
            file.write_all(line)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// `[ep, id, title]` of a well-formed line (non-empty id; the title keeps any
/// further tabs, as ani-cli's `read -r ep id title` does).
fn fields(line: &[u8]) -> Option<[&[u8]; 3]> {
    let mut parts = line.splitn(3, |&b| b == b'\t');
    let fields = [parts.next()?, parts.next()?, parts.next()?];
    (!fields[1].is_empty()).then_some(fields)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn temp_hist(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ani-gui-hist-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("ani-cli").join("ani-hsts")
    }

    fn entry(ep: &str, id: &str, title: &str) -> HistoryEntry {
        HistoryEntry {
            ep: ep.into(),
            id: id.into(),
            title: title.into(),
        }
    }

    #[test]
    fn record_history_replaces_and_appends() {
        let path = temp_hist("replace");
        assert!(read(&path).is_empty());

        record(&path, &entry("1", "frieren-481", "Frieren")).unwrap();
        record(&path, &entry("3", "chiikawa-948", "Chiikawa")).unwrap();
        record(
            &path,
            &entry("2", "frieren-481", "Frieren: Beyond Journey's End"),
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "2\tfrieren-481\tFrieren: Beyond Journey's End\n3\tchiikawa-948\tChiikawa\n"
        );
        assert_eq!(
            read(&path),
            [
                entry("2", "frieren-481", "Frieren: Beyond Journey's End"),
                entry("3", "chiikawa-948", "Chiikawa"),
            ]
        );
        // Tabs and newlines in a title would corrupt the line format.
        record(&path, &entry("4", "x-1", "A\tB\nC")).unwrap();
        assert_eq!(read(&path)[2], entry("4", "x-1", "A B C"));
        assert!(record(&path, &entry("1", "", "No id")).is_err());
        assert!(record(&path, &entry("1", "a\tb", "Bad id")).is_err());
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn record_history_preserves_other_lines() {
        let path = temp_hist("preserve");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "5\tchiikawa-948\tChiikawa\n\
            \n\
            garbage without tabs\n\
            1\tmushoku-452\tMushoku Tensei: Jobless Reincarnation\n\
            \t\t\n\
            7\takane-241\tAkane-banashi & \"Rakugo\" | 100% \\o/\n\
            2\ttrailing-1\tNo final newline";
        fs::write(&path, original).unwrap();

        assert_eq!(
            read(&path),
            [
                entry("5", "chiikawa-948", "Chiikawa"),
                entry("1", "mushoku-452", "Mushoku Tensei: Jobless Reincarnation"),
                entry("7", "akane-241", "Akane-banashi & \"Rakugo\" | 100% \\o/"),
                entry("2", "trailing-1", "No final newline"),
            ]
        );

        record(&path, &entry("2", "mushoku-452", "Mushoku Tensei")).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "5\tchiikawa-948\tChiikawa\n\
            \n\
            garbage without tabs\n\
            2\tmushoku-452\tMushoku Tensei\n\
            \t\t\n\
            7\takane-241\tAkane-banashi & \"Rakugo\" | 100% \\o/\n\
            2\ttrailing-1\tNo final newline\n"
        );

        record(&path, &entry("1", "new-9", "New")).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .ends_with("2\ttrailing-1\tNo final newline\n1\tnew-9\tNew\n")
        );
        // Written via a temp file that does not linger.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["ani-hsts"]);
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}
