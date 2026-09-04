//! The gapless fallback mux (DESIGN §10.5) and the natural-order comparator it depends on.
//!
//! # Why not `-f concat`
//!
//! ffmpeg's concat *demuxer* pads every input to its container-reported duration. For the
//! ~8.0 s audio-aligned TS segments StreamingCommunity serves, the container says ~8.064 s, so
//! every join gains a ~64 ms A/V gap and drops about one frame. Over a 45-minute episode that is
//! hundreds of stutters and audible drift. Binary-concatenating the raw segments and handing
//! ffmpeg **one** continuous input preserves the source timestamps and produces a gapless file, so
//! `-f concat` must never appear in the argv — [`gapless_mux`]'s own test asserts it does not.
//!
//! # Why natural order and not mtime
//!
//! `N_m3u8DL-RE` downloads segments with `--thread-count 16`, so mtimes are non-monotonic: sorting
//! by mtime scrambles playback. Filenames are what carry the order, and they are numeric, so the
//! comparator has to be numeric-aware — `seg2` before `seg10`, not after it (legacy
//! `app/ytdl.py:800-812`).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aulos_provider::proc::{Child, SpawnSpec};
use aulos_provider::provider::ProviderError;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The extensions `N_m3u8DL-RE` leaves in its segment directory (legacy `app/ytdl.py:805`).
pub const SEGMENT_EXTENSIONS: [&str; 5] = [".m4s", ".ts", ".mp4", ".m4a", ".aac"];

/// The name of the binary concatenation, inside the segment directory (legacy `_merged.ts`).
pub const MERGED_NAME: &str = "_merged.ts";

/// The copy buffer, 1 MiB — legacy's `shutil.copyfileobj(..., length=1024 * 1024)`.
const COPY_CHUNK: usize = 1024 * 1024;

/// The ffmpeg remux timeout (legacy `timeout=600`).
pub const MUX_TIMEOUT: Duration = Duration::from_secs(600);

/// One run of characters in a filename: a decimal number, or anything else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tok<'a> {
    /// A maximal run of ASCII digits, kept as text so the padding is still visible.
    Num(&'a str),
    /// A maximal run of non-digits.
    Text(&'a str),
}

/// Splits `s` into maximal digit and non-digit runs.
fn tokens(s: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let digit = bytes[i].is_ascii_digit();
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() == digit {
            j += 1;
        }
        // `i` and `j` sit on ASCII boundaries or at a byte that starts a UTF-8 sequence: a
        // continuation byte is never an ASCII digit, so a multi-byte character is always wholly
        // inside one `Text` run.
        let run = &s[i..j];
        out.push(if digit { Tok::Num(run) } else { Tok::Text(run) });
        i = j;
    }
    out
}

/// Compares two digit runs by numeric value, then by padding.
///
/// The padding tie-break is what makes this a **total** order rather than merely a useful one:
/// `"02"` and `"2"` are numerically equal but are different filenames, and a comparator that
/// called them `Equal` would make `sort_by` order-dependent. The shorter spelling sorts first.
fn cmp_num(a: &str, b: &str) -> Ordering {
    let (ta, tb) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    ta.len()
        .cmp(&tb.len())
        .then_with(|| ta.cmp(tb))
        .then_with(|| a.len().cmp(&b.len()))
}

/// A numeric-aware filename comparator: `seg2 < seg10`, and a total order.
///
/// Hand-written rather than pulled from `natord`, which has been unmaintained since ~2015
/// (DESIGN §18.6). It is a plain lexicographic comparison of the [`tokens`] sequence, where a
/// digit run compares numerically, a non-digit run compares bytewise, and a digit run sorts before
/// a non-digit one — every part of which is a total order, so the composition is too. Equal
/// therefore means *byte-equal*, because a string's token sequence determines the string.
#[must_use]
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (ta, tb) = (tokens(a), tokens(b));
    for (x, y) in ta.iter().zip(tb.iter()) {
        let ord = match (x, y) {
            (Tok::Num(p), Tok::Num(q)) => cmp_num(p, q),
            (Tok::Text(p), Tok::Text(q)) => p.as_bytes().cmp(q.as_bytes()),
            (Tok::Num(_), Tok::Text(_)) => Ordering::Less,
            (Tok::Text(_), Tok::Num(_)) => Ordering::Greater,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ta.len().cmp(&tb.len())
}

/// Every segment under `seg_dir`, in natural filename order.
///
/// Recursive, like legacy's `os.walk`, and sorted by **basename** the way legacy's `_natural_key`
/// was; directory entries are visited in natural order too, so two files that share a basename in
/// different sub-directories still land in a deterministic order (legacy left that to the walk).
///
/// [`MERGED_NAME`] is skipped. Legacy would have re-concatenated its own previous output on a
/// second attempt in the same directory; nothing depends on that, and skipping it is the
/// difference between a retry that works and one that produces a file twice as long.
#[must_use]
pub fn collect_segments(seg_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk(seg_dir, &mut files);
    files.sort_by(|a, b| {
        let (an, bn) = (file_name(a), file_name(b));
        natural_cmp(an, bn)
    });
    files
}

/// The file name as a `&str`, or `""` for a path that has none.
fn file_name(p: &Path) -> &str {
    p.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("")
}

/// Depth-first walk in natural directory order, collecting segment files.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    children.sort_by(|a, b| natural_cmp(file_name(a), file_name(b)));
    for child in children {
        if child.is_dir() {
            walk(&child, out);
        } else if is_segment(file_name(&child)) {
            out.push(child);
        }
    }
}

/// Whether a file name is one of the segment kinds and not the merged output.
fn is_segment(name: &str) -> bool {
    name != MERGED_NAME && SEGMENT_EXTENSIONS.iter().any(|e| name.ends_with(e))
}

/// Binary-concatenates `files` into `out` in 1 MiB chunks, returning the byte count.
///
/// # Errors
/// [`ProviderError::Disk`] on `ENOSPC` and [`ProviderError::Postprocessing`] for any other I/O
/// failure, so a full disk is reported as a full disk.
pub async fn concat_segments(files: &[PathBuf], out: &Path) -> Result<u64, ProviderError> {
    let mut sink = tokio::fs::File::create(out)
        .await
        .map_err(|e| io(&e, out))?;
    let mut buf = vec![0_u8; COPY_CHUNK];
    let mut total = 0_u64;
    for path in files {
        let mut src = tokio::fs::File::open(path)
            .await
            .map_err(|e| io(&e, path))?;
        loop {
            let n = src.read(&mut buf).await.map_err(|e| io(&e, path))?;
            if n == 0 {
                break;
            }
            sink.write_all(&buf[..n]).await.map_err(|e| io(&e, out))?;
            total += n as u64;
        }
    }
    sink.flush().await.map_err(|e| io(&e, out))?;
    Ok(total)
}

/// Maps an I/O failure onto the provider taxonomy, keeping `ENOSPC` distinguishable.
fn io(e: &std::io::Error, path: &Path) -> ProviderError {
    let msg = format!("{}: {e}", path.display());
    if e.raw_os_error() == Some(nix_enospc()) {
        ProviderError::Disk(msg)
    } else {
        ProviderError::Postprocessing(msg)
    }
}

/// `ENOSPC`, without taking a `nix`/`libc` dependency this crate's DESIGN §3 row does not budget
/// for. The value is 28 on every platform this server runs on.
const fn nix_enospc() -> i32 {
    28
}

/// The gapless fallback: concatenate the segments, then remux once with ffmpeg.
///
/// Returns the size of `out` in bytes.
///
/// `ffmpeg_bin` is a parameter rather than the literal `"ffmpeg"` so the tests can point it at a
/// fixture script; PLAN WP-09's signature is `gapless_mux(seg_dir, out)` and this adds exactly
/// that one argument (see `docs/INTEGRATION-NOTES.md`, WP-09).
///
/// # Errors
/// [`ProviderError::Postprocessing`] when the directory holds no segments, when ffmpeg fails or
/// times out, or when it exits `0` without producing `out`; [`ProviderError::ToolMissing`] when
/// ffmpeg is not installed.
pub async fn gapless_mux(
    ffmpeg_bin: &std::ffi::OsStr,
    seg_dir: &Path,
    out: &Path,
) -> Result<u64, ProviderError> {
    let files = collect_segments(seg_dir);
    if files.is_empty() {
        return Err(ProviderError::Postprocessing(
            crate::engines::MSG_NO_SEGMENTS.to_owned(),
        ));
    }
    let merged = seg_dir.join(MERGED_NAME);
    let bytes = concat_segments(&files, &merged).await?;
    tracing::info!(
        segments = files.len(),
        bytes,
        merged = %merged.display(),
        "binary-concatenated the segment directory in natural order"
    );

    let spec = mux_spec(ffmpeg_bin, &merged, out);
    let mut child = Child::spawn(&spec)?;
    let waited = tokio::time::timeout(MUX_TIMEOUT, child.wait()).await;
    let status = match waited {
        Ok(status) => status?,
        Err(_) => {
            child.kill_group().await;
            return Err(ProviderError::Postprocessing(format!(
                "the gapless mux of {} timed out after {} s",
                seg_dir.display(),
                MUX_TIMEOUT.as_secs()
            )));
        }
    };
    let size = tokio::fs::metadata(out).await.ok().map(|m| m.len());
    match (status.success(), size) {
        (true, Some(size)) => Ok(size),
        _ => {
            let tail = crate::engines::settled_tail(&mut child).await;
            tracing::error!(status = ?status.code(), tail = %tail, "the gapless mux failed");
            Err(ProviderError::Postprocessing(
                crate::engines::MSG_MUX_FAILED.to_owned(),
            ))
        }
    }
}

/// The single-input remux (DESIGN §10.5, legacy `app/ytdl.py:823-831`).
///
/// `-map 0` keeps every stream, `-c copy` re-containers without re-encoding, `aac_adtstoasc`
/// converts the ADTS audio headers TS carries into the ASC form mp4 needs, and `+faststart` moves
/// the index to the front so the file starts playing before it is fully fetched.
#[must_use]
pub fn mux_spec(ffmpeg_bin: &std::ffi::OsStr, merged: &Path, out: &Path) -> SpawnSpec {
    SpawnSpec::new("ffmpeg", ffmpeg_bin)
        .arg("-y")
        .arg("-i")
        .arg(merged)
        .arg("-map")
        .arg("0")
        .arg("-c")
        .arg("copy")
        .arg("-bsf:a")
        .arg("aac_adtstoasc")
        .arg("-movflags")
        .arg("+faststart")
        .arg(out)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;
    use crate::testing::{fixture_bin, real_ffmpeg, write};

    #[test]
    fn seg2_sorts_before_seg10() {
        assert_eq!(natural_cmp("seg2.ts", "seg10.ts"), Ordering::Less);
        assert_eq!(natural_cmp("seg10.ts", "seg2.ts"), Ordering::Greater);
    }

    #[test]
    fn the_comparator_table() {
        let cases: [(&str, &str, Ordering); 14] = [
            ("seg2.ts", "seg10.ts", Ordering::Less),
            ("seg002.ts", "seg10.ts", Ordering::Less),
            ("seg2.ts", "seg002.ts", Ordering::Less), // equal value: less padding sorts first
            ("seg002.ts", "seg002.ts", Ordering::Equal),
            ("1.ts", "2.ts", Ordering::Less),
            ("9.ts", "10.ts", Ordering::Less),
            ("099.ts", "100.ts", Ordering::Less),
            ("a.ts", "b.ts", Ordering::Less),
            ("seg1.ts", "seg1.m4s", Ordering::Greater), // ".ts" > ".m4s" bytewise
            ("seg1", "seg1.ts", Ordering::Less),        // a prefix sorts first
            ("1abc", "abc", Ordering::Less),            // digits sort before text
            ("abc", "1abc", Ordering::Greater),
            ("part2seg10", "part2seg9", Ordering::Greater),
            ("part10seg1", "part9seg1", Ordering::Greater),
        ];
        for (a, b, want) in cases {
            assert_eq!(natural_cmp(a, b), want, "{a} vs {b}");
        }
    }

    #[test]
    fn a_scrambled_directory_sorts_numerically() {
        let mut names = vec![
            "seg10.ts",
            "seg2.ts",
            "seg1.ts",
            "seg20.ts",
            "seg3.ts",
            "seg100.ts",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            [
                "seg1.ts",
                "seg2.ts",
                "seg3.ts",
                "seg10.ts",
                "seg20.ts",
                "seg100.ts"
            ]
        );
    }

    #[test]
    fn multibyte_names_are_compared_without_splitting_a_character() {
        // The tokenizer walks bytes; a continuation byte is never an ASCII digit, so "è" stays in
        // one `Text` run and the comparison is still byte-lexicographic.
        assert_eq!(natural_cmp("è1.ts", "è2.ts"), Ordering::Less);
        assert_eq!(natural_cmp("è1.ts", "è1.ts"), Ordering::Equal);
        assert_ne!(natural_cmp("è1.ts", "e1.ts"), Ordering::Equal);
    }

    #[test]
    fn only_segment_files_are_collected() {
        for name in ["a.m4s", "a.ts", "a.mp4", "a.m4a", "a.aac"] {
            assert!(is_segment(name), "{name}");
        }
        for name in ["a.txt", "a.json", "a", "a.TS", MERGED_NAME] {
            assert!(!is_segment(name), "{name}");
        }
    }

    #[tokio::test]
    async fn the_concatenation_is_exactly_the_natural_order_concatenation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seg = dir.path().join("Episode");
        std::fs::create_dir_all(&seg).expect("mkdir");
        // Written newest-first so the mtimes are the *reverse* of the natural order: a comparator
        // that fell back to mtime would produce "cba".
        write(&seg.join("seg10.ts"), b"c");
        std::thread::sleep(std::time::Duration::from_millis(10));
        write(&seg.join("seg2.ts"), b"b");
        std::thread::sleep(std::time::Duration::from_millis(10));
        write(&seg.join("seg1.ts"), b"a");
        write(&seg.join("notes.txt"), b"X");

        let files = collect_segments(&seg);
        assert_eq!(files.len(), 3, "{files:?}");
        let merged = seg.join(MERGED_NAME);
        let n = concat_segments(&files, &merged).await.expect("concat");
        assert_eq!(n, 3);
        assert_eq!(std::fs::read(&merged).expect("read"), b"abc");
    }

    #[tokio::test]
    async fn the_concatenation_walks_sub_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seg = dir.path().join("Episode");
        std::fs::create_dir_all(seg.join("audio")).expect("mkdir");
        std::fs::create_dir_all(seg.join("video")).expect("mkdir");
        write(&seg.join("video/seg1.ts"), b"v1");
        write(&seg.join("video/seg2.ts"), b"v2");
        write(&seg.join("audio/seg1.m4a"), b"a1");
        let files = collect_segments(&seg);
        assert_eq!(files.len(), 3);
        // Basename order, exactly as legacy's `_natural_key(os.path.basename)`: the two `seg1`
        // files come first (audio before video, from the natural directory walk), then `seg2`.
        assert_eq!(
            files.iter().map(|p| file_name(p)).collect::<Vec<_>>(),
            ["seg1.m4a", "seg1.ts", "seg2.ts"]
        );
    }

    #[tokio::test]
    async fn a_chunk_boundary_does_not_corrupt_the_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seg = dir.path().join("Episode");
        std::fs::create_dir_all(&seg).expect("mkdir");
        // Two segments either side of the 1 MiB copy buffer.
        let a = vec![b'a'; COPY_CHUNK + 7];
        let b = vec![b'b'; 13];
        write(&seg.join("seg1.ts"), &a);
        write(&seg.join("seg2.ts"), &b);
        let merged = seg.join(MERGED_NAME);
        let n = concat_segments(&collect_segments(&seg), &merged)
            .await
            .expect("concat");
        assert_eq!(n as usize, a.len() + b.len());
        let got = std::fs::read(&merged).expect("read");
        assert_eq!(&got[..a.len()], &a[..]);
        assert_eq!(&got[a.len()..], &b[..]);
    }

    #[test]
    fn the_argv_never_contains_f_concat() {
        let spec = mux_spec(
            OsStr::new("ffmpeg"),
            Path::new("/tmp/Ep/_merged.ts"),
            Path::new("/out/Ep.mp4"),
        );
        assert_eq!(
            spec.argv(),
            [
                "ffmpeg",
                "-y",
                "-i",
                "/tmp/Ep/_merged.ts",
                "-map",
                "0",
                "-c",
                "copy",
                "-bsf:a",
                "aac_adtstoasc",
                "-movflags",
                "+faststart",
                "/out/Ep.mp4",
            ]
        );
        assert!(
            !spec.argv().windows(2).any(|w| w == ["-f", "concat"]),
            "the concat demuxer injects a ~64 ms gap per join"
        );
    }

    #[tokio::test]
    async fn the_mux_spawns_exactly_one_ffmpeg_and_never_asks_for_concat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seg = dir.path().join("Episode");
        std::fs::create_dir_all(&seg).expect("mkdir");
        write(&seg.join("seg1.ts"), b"aa");
        write(&seg.join("seg2.ts"), b"bb");
        let out = dir.path().join("Episode.mp4");

        let size = gapless_mux(fixture_bin("fake_ffmpeg_mux.sh").as_os_str(), &seg, &out)
            .await
            .expect("the mux must succeed");
        assert!(size > 0);
        assert!(out.is_file());

        let log = std::fs::read_to_string(dir.path().join("ffmpeg-invocations.log")).expect("log");
        let runs: Vec<&str> = log.lines().filter(|l| *l == "--").collect();
        assert_eq!(runs.len(), 1, "exactly one ffmpeg process:\n{log}");
        assert!(!log.contains("concat"), "{log}");
        // The merged file really was the single input.
        assert!(log.contains("_merged.ts"), "{log}");
    }

    #[tokio::test]
    async fn an_empty_segment_directory_reports_the_legacy_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seg = dir.path().join("Episode");
        std::fs::create_dir_all(&seg).expect("mkdir");
        let err = gapless_mux(OsStr::new("ffmpeg"), &seg, &dir.path().join("Episode.mp4"))
            .await
            .expect_err("no segments");
        assert_eq!(err.to_string(), crate::engines::MSG_NO_SEGMENTS);
    }

    #[tokio::test]
    async fn a_failing_ffmpeg_reports_the_legacy_muxing_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seg = dir.path().join("Episode");
        std::fs::create_dir_all(&seg).expect("mkdir");
        write(&seg.join("seg1.ts"), b"aa");
        let err = gapless_mux(
            fixture_bin("fake_ffmpeg_fail.sh").as_os_str(),
            &seg,
            &dir.path().join("Episode.mp4"),
        )
        .await
        .expect_err("ffmpeg failed");
        assert_eq!(err.to_string(), crate::engines::MSG_MUX_FAILED);
        assert_eq!(
            err.code(),
            aulos_core::error::ErrorCode::PostprocessingFailed
        );
    }

    /// The one test that needs a real ffmpeg. Skipped, loudly, when there is none: CI has no
    /// ffmpeg and this asserts something the fixture scripts cannot — that the concatenation is
    /// something ffmpeg will actually remux.
    #[tokio::test]
    async fn a_real_ffmpeg_remuxes_the_concatenation() {
        let Some(ffmpeg) = real_ffmpeg() else {
            eprintln!("skipping: no ffmpeg on PATH or in /opt/homebrew/bin");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let seg = dir.path().join("Episode");
        std::fs::create_dir_all(&seg).expect("mkdir");

        // Generate two real MPEG-TS segments with ffmpeg itself, so the fixture is not a binary
        // blob in the repository.
        for (i, start) in [(1_u32, "0"), (2, "1")] {
            let spec = SpawnSpec::new("ffmpeg", &ffmpeg)
                .args([
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1",
                    "-ss",
                    start,
                    "-c:a",
                    "aac",
                    "-f",
                    "mpegts",
                ])
                .arg(seg.join(format!("seg{i}.ts")));
            let mut child = Child::spawn(&spec).expect("spawn ffmpeg");
            let status = child.wait().await.expect("wait");
            assert!(status.success(), "generating seg{i}.ts failed");
        }

        let out = dir.path().join("Episode.mp4");
        let size = gapless_mux(ffmpeg.as_os_str(), &seg, &out)
            .await
            .expect("a real remux");
        assert!(size > 0, "the remuxed file must not be empty");
        assert_eq!(
            std::fs::metadata(&out).expect("stat").len(),
            size,
            "the reported size is the file's size"
        );
    }
}

#[cfg(test)]
mod proptests {
    use std::cmp::Ordering;

    use proptest::prelude::*;

    use super::natural_cmp;

    /// Names built from the alphabet that actually matters: digits, a couple of letters, a dot and
    /// a dash. A wholly random `String` almost never produces two comparable names.
    fn name() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just('0'),
                Just('1'),
                Just('2'),
                Just('9'),
                Just('a'),
                Just('b'),
                Just('.'),
                Just('-'),
            ],
            0..8,
        )
        .prop_map(|cs| cs.into_iter().collect())
    }

    proptest! {
        #[test]
        fn it_is_reflexive(a in name()) {
            prop_assert_eq!(natural_cmp(&a, &a), Ordering::Equal);
        }

        #[test]
        fn it_is_antisymmetric(a in name(), b in name()) {
            prop_assert_eq!(natural_cmp(&a, &b), natural_cmp(&b, &a).reverse());
        }

        #[test]
        fn equal_means_byte_equal(a in name(), b in name()) {
            prop_assert_eq!(natural_cmp(&a, &b) == Ordering::Equal, a == b);
        }

        #[test]
        fn it_is_transitive(a in name(), b in name(), c in name()) {
            let (ab, bc) = (natural_cmp(&a, &b), natural_cmp(&b, &c));
            if ab == Ordering::Less && bc == Ordering::Less {
                prop_assert_eq!(natural_cmp(&a, &c), Ordering::Less);
            }
            if ab == Ordering::Greater && bc == Ordering::Greater {
                prop_assert_eq!(natural_cmp(&a, &c), Ordering::Greater);
            }
        }

        #[test]
        fn sorting_is_a_permutation_and_is_idempotent(mut names in proptest::collection::vec(name(), 0..12)) {
            let mut once = names.clone();
            once.sort_by(|a, b| natural_cmp(a, b));
            let mut twice = once.clone();
            twice.sort_by(|a, b| natural_cmp(a, b));
            prop_assert_eq!(&once, &twice);
            names.sort();
            let mut sorted_once = once.clone();
            sorted_once.sort();
            prop_assert_eq!(names, sorted_once, "sorting must not lose or invent a name");
        }
    }
}
