//! Synced lyrics from `.lrc` sidecar files.
//!
//! An LRC file is plain text with a timestamp before each line: `[01:23.45]`
//! or `[01:23.450]`, or several stamps on one line for a repeated chorus. Tags
//! such as `[ar:Artist]` describe the file and are ignored, except `[offset:]`,
//! which shifts every stamp. Word-level stamps (`<01:23.45>`) are dropped so
//! the text reads plainly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Larger files are not lyrics. A real song's sheet is a few kilobytes.
pub const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// When the line starts, from the start of the song.
    pub at_ms: i64,
    /// Empty for a stamp with no words, which marks an instrumental gap.
    pub text: String,
}

/// Parse an LRC document. Lines come back sorted by time. A document with no
/// timed lines comes back empty, which is how a plain-text `.lrc` is refused.
pub fn parse(text: &str) -> Vec<Line> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut offset = 0i64;
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(tagged) = rest.strip_prefix('[') {
            let Some(end) = tagged.find(']') else { break };
            let tag = &tagged[..end];
            if let Some(ms) = parse_stamp(tag) {
                stamps.push(ms);
            } else if let Some(value) = tag.strip_prefix("offset:") {
                offset = value.trim().parse().unwrap_or(0);
            }
            rest = tagged[end + 1..].trim_start();
        }
        if stamps.is_empty() {
            continue;
        }
        let words = strip_word_stamps(rest);
        for stamp in stamps {
            lines.push(Line {
                at_ms: (stamp - offset).max(0),
                text: words.clone(),
            });
        }
    }
    // Stable, so two lines at the same moment keep the order they were written in.
    lines.sort_by_key(|line| line.at_ms);
    lines
}

/// The index of the line on screen at `position_ms`: the last line that has
/// started. `None` before the first line. `lines` must be sorted, as `parse`
/// leaves them.
pub fn current(lines: &[Line], position_ms: i64) -> Option<usize> {
    lines
        .partition_point(|line| line.at_ms <= position_ms)
        .checked_sub(1)
}

/// Split `files` into the files to import and the `.lrc` sidecars that belong
/// to them. A sidecar belongs to every other file in the same folder with the
/// same name, ignoring case and extension. A sidecar with no such file is
/// discarded, and so is one whose text has no timed lines.
pub struct Pairing {
    pub files: Vec<PathBuf>,
    /// Song path to the sidecar that carries its lyrics.
    pub sidecars: HashMap<PathBuf, PathBuf>,
    pub discarded: usize,
}

pub fn pair(files: Vec<PathBuf>) -> Pairing {
    let (sidecars, songs): (Vec<PathBuf>, Vec<PathBuf>) = files.into_iter().partition(|path| {
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lrc"))
    });
    let mut by_name: HashMap<(PathBuf, String), Vec<PathBuf>> = HashMap::new();
    for song in &songs {
        if let Some(key) = name_key(song) {
            by_name.entry(key).or_default().push(song.clone());
        }
    }
    let mut paired = HashMap::new();
    let mut discarded = 0;
    for sidecar in sidecars {
        let matches = name_key(&sidecar).and_then(|key| by_name.get(&key));
        match matches {
            Some(matches) => {
                for song in matches {
                    paired.insert(song.clone(), sidecar.clone());
                }
            }
            None => discarded += 1,
        }
    }
    Pairing {
        files: songs,
        sidecars: paired,
        discarded,
    }
}

/// Read a sidecar for import. `None` when it is too large, unreadable, or has
/// no timed lines, in which case it is discarded.
pub fn read(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if parse(&text).is_empty() {
        return None;
    }
    Some(text)
}

fn name_key(path: &Path) -> Option<(PathBuf, String)> {
    let parent = path.parent()?.to_path_buf();
    let stem = path.file_stem()?.to_string_lossy().to_lowercase();
    Some((parent, stem))
}

/// `mm:ss`, `mm:ss.x`, `mm:ss.xx` or `mm:ss.xxx`, as milliseconds. Hundredths
/// are the usual precision, so `.5` is 500 ms and `.05` is 50 ms.
fn parse_stamp(tag: &str) -> Option<i64> {
    let (minutes, seconds) = tag.split_once(':')?;
    if minutes.is_empty() || !minutes.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (whole, fraction) = match seconds.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (seconds, None),
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut ms = minutes.parse::<i64>().ok()? * 60_000 + whole.parse::<i64>().ok()? * 1000;
    if let Some(fraction) = fraction {
        if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let digits = &fraction[..fraction.len().min(3)];
        let scale = [100, 10, 1][digits.len() - 1];
        ms += digits.parse::<i64>().ok()? * scale;
    }
    Some(ms)
}

/// Remove `<mm:ss.xx>` word stamps from a line of text and tidy the spaces.
fn strip_word_stamps(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('>') {
            Some(close) if parse_stamp(&after[..close]).is_some() => {
                rest = &after[close + 1..];
            }
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_read_hundredths_and_thousandths() {
        assert_eq!(parse_stamp("01:02.03"), Some(62_030));
        assert_eq!(parse_stamp("00:01.5"), Some(1_500));
        assert_eq!(parse_stamp("00:01.050"), Some(1_050));
        assert_eq!(parse_stamp("00:01"), Some(1_000));
        assert_eq!(parse_stamp("ar:Someone"), None);
        assert_eq!(parse_stamp("00:xx"), None);
        assert_eq!(parse_stamp(":01"), None);
    }

    #[test]
    fn a_repeated_line_is_timed_at_each_stamp_in_order() {
        let lines = parse("[00:30.00][01:10.00]Chorus\n[00:05.00]Intro");
        let times: Vec<(i64, &str)> = lines
            .iter()
            .map(|line| (line.at_ms, line.text.as_str()))
            .collect();
        assert_eq!(
            times,
            vec![(5_000, "Intro"), (30_000, "Chorus"), (70_000, "Chorus")]
        );
    }

    #[test]
    fn tags_and_plain_lines_are_not_lyrics() {
        let lines = parse("[ar:Artist]\n[ti:Title]\n[offset:0]\nno stamp here\n[00:01.00]Hello");
        assert_eq!(
            lines,
            vec![Line {
                at_ms: 1_000,
                text: "Hello".into()
            }]
        );
        assert!(parse("just words, no timing").is_empty());
    }

    #[test]
    fn offset_shifts_every_stamp_and_never_goes_negative() {
        // A positive offset brings the lyrics forward, as the LRC convention has it.
        let lines = parse("[offset:+500]\n[00:01.00]A");
        assert_eq!(lines[0].at_ms, 500);
        let lines = parse("[offset:-1000]\n[00:01.00]A");
        assert_eq!(lines[0].at_ms, 2_000);
        let lines = parse("[offset:2000]\n[00:01.00]A");
        assert_eq!(lines[0].at_ms, 0);
    }

    #[test]
    fn word_stamps_are_removed_from_the_text() {
        let lines = parse("[00:01.00]<00:01.00>Hello <00:01.50>world  <bold>");
        assert_eq!(lines[0].text, "Hello world <bold>");
    }

    #[test]
    fn crlf_and_bom_are_handled() {
        let lines = parse("\u{feff}[00:01.00]One\r\n[00:02.00]Two\r\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].text, "Two");
    }

    #[test]
    fn current_line_is_the_last_one_started() {
        let lines = parse("[00:02.00]A\n[00:04.00]B\n[00:04.00]C");
        assert_eq!(current(&lines, 0), None);
        assert_eq!(current(&lines, 2_000), Some(0));
        assert_eq!(current(&lines, 3_999), Some(0));
        assert_eq!(current(&lines, 4_000), Some(2));
        assert_eq!(current(&lines, 99_000), Some(2));
    }

    #[test]
    fn a_sidecar_pairs_with_its_song_and_nothing_else_is_kept() {
        let files = vec![
            PathBuf::from("/music/01. Song.flac"),
            PathBuf::from("/music/01. song.LRC"),
            PathBuf::from("/music/02. Other.lrc"),
            PathBuf::from("/music/03. Plain.mp3"),
            PathBuf::from("/elsewhere/02. Other.flac"),
        ];
        let pairing = pair(files);
        assert_eq!(pairing.discarded, 1);
        assert_eq!(pairing.files.len(), 3);
        assert_eq!(
            pairing.sidecars.get(Path::new("/music/01. Song.flac")),
            Some(&PathBuf::from("/music/01. song.LRC"))
        );
        assert!(
            !pairing
                .sidecars
                .contains_key(Path::new("/music/03. Plain.mp3"))
        );
        assert!(
            !pairing
                .sidecars
                .contains_key(Path::new("/elsewhere/02. Other.flac"))
        );
    }
}
