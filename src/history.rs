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
}
