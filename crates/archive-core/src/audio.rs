//! Music files: what a file says about itself, the cover it carries, and the
//! one re-encode worth making on import.
//!
//! Opus at [`CEILING_BPS`] is the general target. Opus at 128 kbps is close to
//! transparent for music, so the saving is large and the loss is not
//! perceptible. Two kinds of source are re-encoded to it:
//!
//! - Lossless audio (FLAC, ALAC, APE, PCM), which is where the saving is
//!   largest.
//! - Lossy audio above the ceiling, such as a 320 kbps MP3 or 256 kbps AAC.
//!
//! A lossy source at or below the ceiling is kept, since re-encoding it would
//! save little and add a generation. Opus itself is always kept.
//!
//! A candidate replaces the original only when it is smaller and its checks
//! pass: tags, cover art, channel count and length must all survive. Otherwise
//! the original stays and the reason is recorded on the item.
//!
//! Ogg cannot carry a cover as a stream, so an Opus file gets its cover as a
//! `METADATA_BLOCK_PICTURE` tag, the form Vorbis-style players read. The tags
//! and that picture travel in an ffmetadata sidecar rather than on the command
//! line, because a cover alone can exceed what a command line may hold.

use crate::ffmpeg::{self, Tools};
use crate::time;
use base64::Engine;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The Opus bitrate ceiling, for stereo and wider. Lossy audio at or below it
/// is not re-encoded.
pub const CEILING_BPS: i64 = 128_000;
/// Mono and each extra channel get this much. Below the ceiling, so a mono
/// file is not given the stereo budget it does not need.
const PER_CHANNEL_BPS: i64 = 64_000;
/// How far apart the durations of an original and its re-encode may be. Opus
/// adds pre-skip and padding, a few milliseconds; this leaves room for it.
const DURATION_TOLERANCE_MS: i64 = 250;

/// The tags the library reads and searches on. Every field is optional: a
/// file without tags still imports, and its details simply show less.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_no: Option<i64>,
    pub track_total: Option<i64>,
    pub disc_no: Option<i64>,
    pub year: Option<i64>,
    pub genre: Option<String>,
}

/// What ffprobe reports about a music file's audio stream and tags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub tags: Tags,
    /// ffprobe's codec name for the audio stream, e.g. `mp3`, `flac`, `opus`.
    pub codec: String,
    /// Bits per second, when the file states it. Lossless files often do not.
    pub bitrate: Option<i64>,
    pub sample_rate: Option<i64>,
    pub channels: Option<i64>,
    pub duration_ms: Option<i64>,
    /// The file carries cover art. The probe sets this from the picture stream;
    /// the import overwrites it with whether a cover preview was made.
    pub has_cover: bool,
}

/// Read a music file's stream and tags from ffprobe's JSON. `None` when the
/// document has no audio stream. Pure, so it can be tested with fixtures.
pub fn from_json(json: &Value) -> Option<Meta> {
    let streams = json.get("streams")?.as_array()?;
    let stream = streams
        .iter()
        .find(|stream| codec_type(stream) == Some("audio"))?;
    let format = json.get("format");
    let format_tags = format.and_then(|format| format.get("tags"));
    let stream_tags = stream.get("tags");
    // Containers name the same tag differently (ID3 `TPE2` is `album_artist`
    // in one demuxer and `ALBUMARTIST` in another), so each field tries its
    // aliases in order and matches names case-insensitively.
    let tag = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| lookup(format_tags, key).or_else(|| lookup(stream_tags, key)))
    };
    let tags = Tags {
        title: tag(&["title"]),
        artist: tag(&["artist"]),
        album: tag(&["album"]),
        album_artist: tag(&["album_artist", "albumartist", "album artist"]),
        track_no: tag(&["track", "tracknumber"]).and_then(|text| parse_pair(&text).0),
        track_total: tag(&["track", "tracknumber"])
            .and_then(|text| parse_pair(&text).1)
            .or_else(|| tag(&["tracktotal", "totaltracks"]).and_then(|text| leading_int(&text))),
        disc_no: tag(&["disc", "discnumber"]).and_then(|text| parse_pair(&text).0),
        year: tag(&["date", "year", "originaldate"]).and_then(|text| parse_year(&text)),
        genre: tag(&["genre"]),
    };
    Some(Meta {
        tags,
        codec: stream.get("codec_name")?.as_str()?.to_ascii_lowercase(),
        bitrate: number_of(stream.get("bit_rate"))
            .or_else(|| number_of(format.and_then(|format| format.get("bit_rate")))),
        sample_rate: number_of(stream.get("sample_rate")),
        channels: number_of(stream.get("channels")),
        duration_ms: seconds_ms(format.and_then(|format| format.get("duration")))
            .or_else(|| seconds_ms(stream.get("duration"))),
        has_cover: has_attached_picture(streams),
    })
}

/// True when the file carries a real video track. An attached cover picture
/// is not one, and neither is a stream ffprobe reports without a size.
pub fn has_video(json: &Value) -> bool {
    json.get("streams")
        .and_then(Value::as_array)
        .is_some_and(|streams| {
            streams
                .iter()
                .any(|stream| codec_type(stream) == Some("video") && !is_attached_picture(stream))
        })
}

/// Every tag the file states, format-level first, then the audio stream's own.
/// A key the format already gave is not repeated.
pub fn all_tags(json: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    push_tags(
        &mut out,
        json.get("format").and_then(|format| format.get("tags")),
    );
    let audio = json
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| {
            streams
                .iter()
                .find(|stream| codec_type(stream) == Some("audio"))
        });
    push_tags(&mut out, audio.and_then(|stream| stream.get("tags")));
    out
}

fn push_tags(out: &mut Vec<(String, String)>, tags: Option<&Value>) {
    let Some(map) = tags.and_then(Value::as_object) else {
        return;
    };
    for (key, value) in map {
        let Some(text) = value.as_str() else {
            continue;
        };
        if !out.iter().any(|(seen, _)| seen.eq_ignore_ascii_case(key)) {
            out.push((key.clone(), text.to_string()));
        }
    }
}

fn codec_type(stream: &Value) -> Option<&str> {
    stream.get("codec_type")?.as_str()
}

fn is_attached_picture(stream: &Value) -> bool {
    stream
        .get("disposition")
        .and_then(|disposition| disposition.get("attached_pic"))
        .and_then(Value::as_i64)
        == Some(1)
}

fn has_attached_picture(streams: &[Value]) -> bool {
    streams
        .iter()
        .any(|stream| codec_type(stream) == Some("video") && is_attached_picture(stream))
}

/// A tag's text, found by name without regard to case. Blank values count as
/// absent, so an empty `ALBUM=` never shows up as an album called "".
fn lookup(tags: Option<&Value>, key: &str) -> Option<String> {
    tags?
        .as_object()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// ffprobe prints most numbers as strings; accept either form.
fn number_of(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

/// ffprobe prints durations as decimal seconds in a string.
fn seconds_ms(value: Option<&Value>) -> Option<i64> {
    let seconds: f64 = value?.as_str()?.trim().parse().ok()?;
    seconds
        .is_finite()
        .then(|| (seconds * 1000.0).round() as i64)
}

/// `3/12` gives `(Some(3), Some(12))`; `3` gives `(Some(3), None)`. Track and
/// disc numbers are written both ways.
pub fn parse_pair(text: &str) -> (Option<i64>, Option<i64>) {
    let mut parts = text.split('/');
    (
        parts.next().and_then(leading_int),
        parts.next().and_then(leading_int),
    )
}

/// The integer at the start of `text`, so `03` and `3 of 12` both read as 3.
fn leading_int(text: &str) -> Option<i64> {
    let digits: String = text
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// A four-digit year from a date such as `2019`, `2019-05-01` or `05/2019`.
pub fn parse_year(text: &str) -> Option<i64> {
    text.as_bytes()
        .windows(4)
        .find(|window| window.iter().all(u8::is_ascii_digit))
        .and_then(|window| std::str::from_utf8(window).ok()?.parse::<i64>().ok())
        .filter(|year| (1000..=9999).contains(year))
}

/// What the import does with a music file's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    Keep,
    Opus { bps: i64 },
}

/// The policy in one place. See the module docs for the reasoning.
pub fn plan(meta: &Meta) -> Plan {
    let codec = meta.codec.as_str();
    if is_lossless(codec) {
        return Plan::Opus {
            bps: opus_bps(meta.channels),
        };
    }
    let lossy = matches!(codec, "mp3" | "mp2" | "aac" | "vorbis" | "wmav1" | "wmav2");
    match meta.bitrate {
        Some(bps) if lossy && bps > CEILING_BPS => Plan::Opus {
            bps: opus_bps(meta.channels),
        },
        _ => Plan::Keep,
    }
}

/// Codecs that decode to exactly the samples they were made from. WavPack is
/// left out: its lossy mode shares the codec name.
fn is_lossless(codec: &str) -> bool {
    codec.starts_with("pcm_") || matches!(codec, "flac" | "alac" | "ape" | "tta")
}

/// The Opus rate for a source with `channels`: the ceiling for stereo and
/// wider, a smaller share for mono.
pub fn opus_bps(channels: Option<i64>) -> i64 {
    let channels = channels.unwrap_or(2).max(1);
    (channels * PER_CHANNEL_BPS).min(CEILING_BPS)
}

/// A re-encode that passed its checks and is ready to replace the original.
#[derive(Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub ext: &'static str,
    pub mime: &'static str,
    /// What the item's `opt_kind` records, e.g. `flac` or `opus`.
    pub opt_kind: &'static str,
    /// The re-encoded file's own stream and tags, read back from disk.
    pub meta: Meta,
}

/// The outcome of considering one music file for re-encoding.
#[derive(Debug)]
pub enum Attempt {
    /// The policy leaves this file alone. Nothing is recorded.
    Keep,
    /// A re-encode passed every check and should be stored in place of the
    /// original.
    Replaced(Box<Candidate>),
    /// A re-encode was tried and refused, so the original stays. The reason
    /// goes on the item so the details panel can say why.
    Refused(String),
}

/// Re-encode `source` according to [`plan`], if the policy asks for it. The
/// result is written into `staging`, never into the store, and the original
/// is never modified.
pub fn optimize(tools: &Tools, source: &Path, original: &Meta, staging: &Path) -> Attempt {
    let stamp = time::stamp();
    let plan = plan(original);
    let Plan::Opus { bps } = plan else {
        return Attempt::Keep;
    };
    let codec_args = args(&[
        "-c:a",
        "libopus",
        "-b:a",
        &bps.to_string(),
        "-vbr",
        "on",
        "-application",
        "audio",
    ]);
    let dest = staging.join(format!("audio_{stamp}.opus"));
    let sidecar = staging.join(format!("tags_{stamp}.ffmeta"));
    let cover = staging.join(format!("cover_{stamp}.bin"));
    let result = encode_and_check(
        tools,
        source,
        &dest,
        Scratch {
            sidecar: &sidecar,
            cover: &cover,
        },
        plan,
        original,
        &codec_args,
    );
    let _ = std::fs::remove_file(&sidecar);
    let _ = std::fs::remove_file(&cover);
    match result {
        Ok(meta) => Attempt::Replaced(Box::new(Candidate {
            path: dest,
            ext: "opus",
            mime: "audio/ogg",
            opt_kind: "opus",
            meta,
        })),
        Err(reason) => {
            let _ = std::fs::remove_file(&dest);
            Attempt::Refused(reason)
        }
    }
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| part.to_string()).collect()
}

/// Temporary files a re-encode may need, all removed by the caller.
struct Scratch<'a> {
    sidecar: &'a Path,
    cover: &'a Path,
}

fn encode_and_check(
    tools: &Tools,
    source: &Path,
    dest: &Path,
    scratch: Scratch<'_>,
    plan: Plan,
    original: &Meta,
    codec_args: &[String],
) -> std::result::Result<Meta, String> {
    write_sidecar(tools, source, original.has_cover, &scratch)?;
    ffmpeg::transcode_audio(tools, source, scratch.sidecar, dest, codec_args)
        .map_err(|error| error.to_string())?;
    let candidate = ffmpeg::ffprobe_json(tools, dest)
        .as_ref()
        .and_then(from_json)
        .ok_or_else(|| "the re-encode could not be read back".to_string())?;
    if let Some(reason) = mismatch(plan, original, &candidate) {
        return Err(reason);
    }
    let before = std::fs::metadata(source)
        .map_err(|error| error.to_string())?
        .len();
    let after = std::fs::metadata(dest)
        .map_err(|error| error.to_string())?
        .len();
    if after >= before {
        return Err(format!(
            "the re-encode is not smaller ({after} bytes against {before})"
        ));
    }
    Ok(candidate)
}

/// Write the tags, and the cover when there is one, as an ffmetadata sidecar.
fn write_sidecar(
    tools: &Tools,
    source: &Path,
    has_cover: bool,
    scratch: &Scratch<'_>,
) -> std::result::Result<(), String> {
    let json = ffmpeg::ffprobe_json(tools, source)
        .ok_or_else(|| "ffprobe could not read the tags".to_string())?;
    let picture = if has_cover {
        ffmpeg::extract_cover(tools, source, scratch.cover).map_err(|error| error.to_string())?;
        let bytes = std::fs::read(scratch.cover).map_err(|error| error.to_string())?;
        Some(
            picture_text(&bytes)
                .ok_or_else(|| "the cover art is neither JPEG nor PNG".to_string())?,
        )
    } else {
        None
    };
    std::fs::write(
        scratch.sidecar,
        sidecar_text(&all_tags(&json), picture.as_deref()),
    )
    .map_err(|error| error.to_string())
}

/// The base64 `METADATA_BLOCK_PICTURE` value for a JPEG or PNG cover.
fn picture_text(bytes: &[u8]) -> Option<String> {
    let (mime, depth) = if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        ("image/jpeg", 24)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("image/png", 32)
    } else {
        return None;
    };
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    let block = picture_block(mime, width, height, depth, bytes);
    Some(base64::engine::general_purpose::STANDARD.encode(block))
}

/// A FLAC picture block: the layout Vorbis comments use for embedded covers.
/// Type 3 is the front cover; the description and colour count are empty.
pub fn picture_block(mime: &str, width: u32, height: u32, depth: u32, data: &[u8]) -> Vec<u8> {
    let mut block = Vec::with_capacity(32 + mime.len() + data.len());
    block.extend_from_slice(&3u32.to_be_bytes());
    block.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    block.extend_from_slice(mime.as_bytes());
    block.extend_from_slice(&0u32.to_be_bytes());
    block.extend_from_slice(&width.to_be_bytes());
    block.extend_from_slice(&height.to_be_bytes());
    block.extend_from_slice(&depth.to_be_bytes());
    block.extend_from_slice(&0u32.to_be_bytes());
    block.extend_from_slice(&(data.len() as u32).to_be_bytes());
    block.extend_from_slice(data);
    block
}

/// An ffmetadata document carrying `tags` and, optionally, the picture.
/// ffmetadata reserves `=`, `;`, `#`, `\` and newline, so each is escaped.
pub fn sidecar_text(tags: &[(String, String)], picture: Option<&str>) -> String {
    let mut text = String::from(";FFMETADATA1\n");
    for (key, value) in tags {
        text.push_str(&escape(key));
        text.push('=');
        text.push_str(&escape(value));
        text.push('\n');
    }
    if let Some(block) = picture {
        text.push_str("METADATA_BLOCK_PICTURE=");
        text.push_str(&escape(block));
        text.push('\n');
    }
    text
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '=' | ';' | '#' | '\\' | '\n') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Why a re-encode does not match the file it came from, or `None` when it
/// does. Tags, cover art, channels and length must all survive.
fn mismatch(plan: Plan, original: &Meta, candidate: &Meta) -> Option<String> {
    let expected = match plan {
        Plan::Opus { .. } => "opus",
        Plan::Keep => return Some("no re-encode was requested".into()),
    };
    if candidate.codec != expected {
        return Some(format!(
            "the re-encode is {} rather than {expected}",
            candidate.codec
        ));
    }
    if candidate.tags != original.tags {
        return Some("tags changed in the re-encode".into());
    }
    if original.has_cover && !candidate.has_cover {
        return Some("cover art was lost in the re-encode".into());
    }
    if original.channels != candidate.channels {
        return Some("channel count changed in the re-encode".into());
    }
    match (original.duration_ms, candidate.duration_ms) {
        (Some(before), Some(after)) if (before - after).abs() > DURATION_TOLERANCE_MS => {
            Some(format!(
                "length changed by {}ms in the re-encode",
                (before - after).abs()
            ))
        }
        (Some(_), None) => Some("the re-encode has no readable length".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn audio_json(codec: &str, bit_rate: &str, tags: Value) -> Value {
        json!({
            "streams": [{
                "codec_type": "audio",
                "codec_name": codec,
                "sample_rate": "44100",
                "channels": 2,
                "bit_rate": bit_rate,
                "tags": tags,
            }],
            "format": { "duration": "213.500000", "tags": {} }
        })
    }

    fn meta(codec: &str, bitrate: Option<i64>, channels: Option<i64>) -> Meta {
        Meta {
            tags: Tags::default(),
            codec: codec.into(),
            bitrate,
            sample_rate: Some(44_100),
            channels,
            duration_ms: Some(213_500),
            has_cover: false,
        }
    }

    #[test]
    fn tags_are_read_case_insensitively_with_aliases() {
        let json = audio_json(
            "mp3",
            "320000",
            json!({
                "TITLE": " Song ",
                "Artist": "Band",
                "album": "Record",
                "ALBUMARTIST": "Band",
                "track": "3/12",
                "disc": "1",
                "date": "2019-05-01",
                "genre": "Rock"
            }),
        );
        let meta = from_json(&json).expect("an audio stream");
        assert_eq!(meta.tags.title.as_deref(), Some("Song"));
        assert_eq!(meta.tags.artist.as_deref(), Some("Band"));
        assert_eq!(meta.tags.album.as_deref(), Some("Record"));
        assert_eq!(meta.tags.album_artist.as_deref(), Some("Band"));
        assert_eq!(meta.tags.track_no, Some(3));
        assert_eq!(meta.tags.track_total, Some(12));
        assert_eq!(meta.tags.disc_no, Some(1));
        assert_eq!(meta.tags.year, Some(2019));
        assert_eq!(meta.tags.genre.as_deref(), Some("Rock"));
        assert_eq!(meta.bitrate, Some(320_000));
        assert_eq!(meta.duration_ms, Some(213_500));
    }

    #[test]
    fn blank_tags_count_as_missing() {
        let json = audio_json("flac", "0", json!({ "ALBUM": "   ", "TITLE": "" }));
        let meta = from_json(&json).unwrap();
        assert_eq!(meta.tags.album, None);
        assert_eq!(meta.tags.title, None);
    }

    #[test]
    fn every_tag_is_kept_once_format_first() {
        let json = json!({
            "streams": [{
                "codec_type": "audio",
                "tags": { "TITLE": "Stream title", "comment": "from stream" }
            }],
            "format": { "tags": { "title": "Format title", "lyrics": "la la" } }
        });
        // Order is the map's, so compare as sets: the content is what matters.
        let mut tags = all_tags(&json);
        tags.sort();
        assert_eq!(
            tags,
            vec![
                ("comment".to_string(), "from stream".to_string()),
                ("lyrics".to_string(), "la la".to_string()),
                ("title".to_string(), "Format title".to_string()),
            ]
        );
    }

    #[test]
    fn an_attached_cover_is_not_a_video_track() {
        let json = json!({
            "streams": [
                { "codec_type": "audio", "codec_name": "mp3" },
                { "codec_type": "video", "codec_name": "mjpeg",
                  "disposition": { "attached_pic": 1 } }
            ],
            "format": {}
        });
        assert!(!has_video(&json));
        assert!(from_json(&json).unwrap().has_cover);
    }

    #[test]
    fn a_real_video_track_is_reported() {
        let json = json!({
            "streams": [
                { "codec_type": "video", "codec_name": "theora" },
                { "codec_type": "audio", "codec_name": "vorbis" }
            ],
            "format": {}
        });
        assert!(has_video(&json));
    }

    #[test]
    fn no_audio_stream_is_no_music() {
        let json = json!({ "streams": [{ "codec_type": "video" }], "format": {} });
        assert!(from_json(&json).is_none());
    }

    #[test]
    fn years_and_pairs_are_parsed_from_messy_text() {
        assert_eq!(parse_year("2019-05-01"), Some(2019));
        assert_eq!(parse_year("05/2019"), Some(2019));
        assert_eq!(parse_year("no year"), None);
        assert_eq!(parse_pair("7"), (Some(7), None));
        assert_eq!(parse_pair("7/10"), (Some(7), Some(10)));
        assert_eq!(parse_pair("3 of 12"), (Some(3), None));
        assert_eq!(parse_pair(""), (None, None));
    }

    #[test]
    fn lossless_sources_go_to_the_opus_ceiling() {
        for codec in [
            "flac",
            "alac",
            "ape",
            "tta",
            "pcm_s16le",
            "pcm_s24be",
            "pcm_f32le",
        ] {
            assert_eq!(
                plan(&meta(codec, None, Some(2))),
                Plan::Opus { bps: CEILING_BPS },
                "{codec}"
            );
        }
    }

    #[test]
    fn opus_is_never_re_encoded_and_wavpack_is_not_assumed_lossless() {
        assert_eq!(plan(&meta("opus", Some(900_000), Some(2))), Plan::Keep);
        // WavPack's lossy mode shares the codec name, so it is left alone.
        assert_eq!(plan(&meta("wavpack", Some(900_000), Some(2))), Plan::Keep);
    }

    #[test]
    fn lossy_at_or_below_the_ceiling_is_kept() {
        // A 128 kbps MP3 gains little from Opus and would lose a generation.
        assert_eq!(plan(&meta("mp3", Some(128_000), Some(2))), Plan::Keep);
        assert_eq!(plan(&meta("aac", Some(96_000), Some(2))), Plan::Keep);
    }

    #[test]
    fn lossy_above_the_ceiling_is_brought_down_to_it() {
        assert_eq!(
            plan(&meta("mp3", Some(320_000), Some(2))),
            Plan::Opus { bps: CEILING_BPS }
        );
        assert_eq!(
            plan(&meta("aac", Some(256_000), Some(2))),
            Plan::Opus { bps: CEILING_BPS }
        );
        assert_eq!(
            plan(&meta("mp3", Some(CEILING_BPS + 1), Some(2))),
            Plan::Opus { bps: CEILING_BPS }
        );
    }

    #[test]
    fn the_rate_follows_the_channels_up_to_the_ceiling() {
        assert_eq!(opus_bps(Some(1)), 64_000);
        assert_eq!(opus_bps(Some(2)), CEILING_BPS);
        assert_eq!(opus_bps(Some(6)), CEILING_BPS);
        assert_eq!(opus_bps(None), CEILING_BPS);
    }

    #[test]
    fn a_lossy_file_with_no_stated_bitrate_is_kept() {
        assert_eq!(plan(&meta("mp3", None, Some(2))), Plan::Keep);
    }

    #[test]
    fn sidecar_escapes_what_ffmetadata_reserves() {
        let tags = vec![("comment".to_string(), "a=b;c#d\\e\nf".to_string())];
        assert_eq!(
            sidecar_text(&tags, None),
            ";FFMETADATA1\ncomment=a\\=b\\;c\\#d\\\\e\\\nf\n"
        );
        let with_cover = sidecar_text(&[], Some("QUJD=="));
        assert_eq!(
            with_cover,
            ";FFMETADATA1\nMETADATA_BLOCK_PICTURE=QUJD\\=\\=\n"
        );
    }

    #[test]
    fn a_picture_block_follows_the_flac_layout() {
        let block = picture_block("image/png", 2, 3, 32, &[9, 9]);
        assert_eq!(&block[0..4], &3u32.to_be_bytes());
        assert_eq!(&block[4..8], &9u32.to_be_bytes());
        assert_eq!(&block[8..17], b"image/png");
        assert_eq!(&block[17..21], &0u32.to_be_bytes());
        assert_eq!(&block[21..25], &2u32.to_be_bytes());
        assert_eq!(&block[25..29], &3u32.to_be_bytes());
        assert_eq!(&block[29..33], &32u32.to_be_bytes());
        assert_eq!(&block[33..37], &0u32.to_be_bytes());
        assert_eq!(&block[37..41], &2u32.to_be_bytes());
        assert_eq!(&block[41..], &[9, 9]);
    }

    #[test]
    fn a_cover_that_is_not_jpeg_or_png_is_refused() {
        assert_eq!(picture_text(b"GIF89a not a cover"), None);
    }

    #[test]
    fn a_matching_reencode_passes_and_a_damaged_one_does_not() {
        let original = Meta {
            tags: Tags {
                title: Some("Song".into()),
                album: Some("Record".into()),
                ..Tags::default()
            },
            has_cover: true,
            ..meta("mp3", Some(320_000), Some(2))
        };
        let good = Meta {
            codec: "opus".into(),
            duration_ms: Some(213_520),
            ..original.clone()
        };
        assert_eq!(
            mismatch(Plan::Opus { bps: CEILING_BPS }, &original, &good),
            None
        );

        let retagged = Meta {
            tags: Tags {
                title: Some("Song".into()),
                ..Tags::default()
            },
            ..good.clone()
        };
        assert!(mismatch(Plan::Opus { bps: CEILING_BPS }, &original, &retagged).is_some());

        let coverless = Meta {
            has_cover: false,
            ..good.clone()
        };
        assert!(mismatch(Plan::Opus { bps: CEILING_BPS }, &original, &coverless).is_some());

        let shorter = Meta {
            duration_ms: Some(200_000),
            ..good.clone()
        };
        assert!(mismatch(Plan::Opus { bps: CEILING_BPS }, &original, &shorter).is_some());

        let wrong_codec = Meta {
            codec: "aac".into(),
            ..good
        };
        assert!(mismatch(Plan::Opus { bps: CEILING_BPS }, &original, &wrong_codec).is_some());
    }
}
