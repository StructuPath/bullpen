//! Archives behind the one read path.
//!
//! Zip, tar, and gzipped tar are recognized by their magic bytes — never
//! their extensions — and render as an entry listing like a directory
//! read. Passing `entry` extracts one member instead, which `read_file`
//! renders as hashline text like any file. Everything is bounded: the
//! listing caps its entry count, an extracted member caps its bytes while
//! streaming — an archive has no business unpacking further than the
//! transcript can carry.

use std::io::Read;
use std::path::Path;

use crate::{ToolError, truncate_middle};

/// Listing cap; the true total is always reported.
const MAX_ENTRIES: usize = 1_000;
/// Extraction cap, enforced while streaming (zip bombs stay in the bottle).
pub(crate) const MAX_ENTRY_BYTES: usize = 262_144; // matches file reads
/// Rendered-listing cap, matching the other read views.
const MAX_OUTPUT_BYTES: usize = 100_000;
/// How much of a tar stream a scan may consume, measured after
/// decompression. Listing or searching a tar walks the whole stream, and
/// without this a small gzip bomb buys unbounded CPU; hitting the bound
/// caps the listing (reported) or fails the entry search (explained).
const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
/// Distinctive marker carried by the cap's io error, so the tar iterator's
/// wrapped failure is recognizable as "capped", not "corrupt".
const SCAN_CAP: &str = "bullpen-archive-scan-cap";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Zip,
    Tar,
    TarGz,
    /// A gzip stream that is not a tar — plain compressed content, read as
    /// text rather than rejected as a malformed archive.
    Gz,
}

/// Recognize an archive by content: zip's `PK` local/empty headers, gzip's
/// two magic bytes, or tar's `ustar` at offset 257. A gzip stream is only
/// a tar.gz if the *decompressed* head carries the tar magic too.
pub(crate) fn detect(path: &Path) -> Option<Kind> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 265];
    let n = file.read(&mut head).ok()?;
    let head = &head[..n];
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Some(Kind::Zip);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        let file = std::fs::File::open(path).ok()?;
        let mut inner = [0u8; 262];
        let mut decoder = flate2::read::GzDecoder::new(file);
        let mut got = 0;
        while got < inner.len() {
            match decoder.read(&mut inner[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => break,
            }
        }
        return Some(if got >= 262 && &inner[257..262] == b"ustar" {
            Kind::TarGz
        } else {
            Kind::Gz
        });
    }
    if n >= 262 && &head[257..262] == b"ustar" {
        return Some(Kind::Tar);
    }
    None
}

/// A reader that refuses to hand out more than `remaining` bytes; the
/// refusal is an io error carrying [`SCAN_CAP`].
struct LimitedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::other(SCAN_CAP));
        }
        let want = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Whether a tar iteration error is the scan cap biting (as opposed to a
/// genuinely corrupt archive).
fn is_scan_cap(e: &std::io::Error) -> bool {
    e.to_string().contains(SCAN_CAP)
}

fn arch_err(path: &Path, e: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(format!("archive {}: {e}", path.display()))
}

/// One listed member: directories render with a trailing slash.
struct Entry {
    name: String,
    size: u64,
    is_dir: bool,
}

/// `total` counts every member scanned; `entries` holds at most
/// [`MAX_ENTRIES`] of them — the cap is applied while scanning, so a
/// million-member archive costs a count, not a vector. `capped` means the
/// scan budget ran out first: the total is a floor, not a count.
fn render(path: &Path, kind: Kind, entries: Vec<Entry>, total: usize, capped: bool) -> String {
    let label = match kind {
        Kind::Zip => "zip",
        Kind::Tar => "tar",
        Kind::TarGz => "tar.gz",
        Kind::Gz => "gzip",
    };
    if entries.is_empty() && !capped {
        return format!("(empty {label} archive {})", path.display());
    }
    let mut out = format!(
        "{label} archive {} ({total}{} entries) — pass `entry` to read one:\n",
        path.display(),
        if capped { "+" } else { "" }
    );
    for entry in &entries {
        if entry.is_dir {
            out.push_str(&format!("  {}\n", entry.name));
        } else {
            out.push_str(&format!("  {}  {} bytes\n", entry.name, entry.size));
        }
    }
    if capped {
        out.push_str(&format!(
            "[scan capped at {} MiB — listing incomplete]\n",
            MAX_SCAN_BYTES / (1024 * 1024)
        ));
    } else if total > entries.len() {
        out.push_str(&format!("[listed the first {MAX_ENTRIES}]\n"));
    }
    truncate_middle(out, MAX_OUTPUT_BYTES)
}

/// Read `reader` into a bounded buffer; `true` when the cap cut it short.
fn bounded_read(mut reader: impl Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let clipped = (&mut reader)
        .take(MAX_ENTRY_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map(|_| buf.len() > MAX_ENTRY_BYTES)?;
    buf.truncate(MAX_ENTRY_BYTES);
    Ok((buf, clipped))
}

fn zip_archive(path: &Path) -> Result<zip::ZipArchive<std::fs::File>, ToolError> {
    let file = std::fs::File::open(path).map_err(|e| arch_err(path, e))?;
    zip::ZipArchive::new(file).map_err(|e| arch_err(path, e))
}

fn tar_archive(path: &Path, kind: Kind) -> Result<tar::Archive<Box<dyn Read>>, ToolError> {
    let file = std::fs::File::open(path).map_err(|e| arch_err(path, e))?;
    let raw: Box<dyn Read> = match kind {
        Kind::TarGz => Box::new(flate2::read::GzDecoder::new(file)),
        _ => Box::new(file),
    };
    // The whole scan — headers plus the payloads the iterator skips over —
    // draws from one decompressed-byte budget, so a small gzip bomb stops
    // at the budget instead of costing unbounded CPU.
    Ok(tar::Archive::new(Box::new(LimitedReader {
        inner: raw,
        remaining: MAX_SCAN_BYTES,
    })))
}

/// The listing view.
pub(crate) fn list(path: &Path, kind: Kind) -> Result<String, ToolError> {
    let (entries, total, capped) = match kind {
        Kind::Gz => {
            return Err(ToolError::InvalidInput(format!(
                "{} is a plain gzip stream, not an archive — read it without \
                 `entry` to see its decompressed content",
                path.display()
            )));
        }
        Kind::Zip => {
            // The central directory answers without touching member data.
            let mut archive = zip_archive(path)?;
            let total = archive.len();
            let mut entries = Vec::new();
            for i in 0..total.min(MAX_ENTRIES) {
                let member = archive.by_index(i).map_err(|e| arch_err(path, e))?;
                entries.push(Entry {
                    name: member.name().to_string(),
                    size: member.size(),
                    is_dir: member.is_dir(),
                });
            }
            (entries, total, false)
        }
        Kind::Tar | Kind::TarGz => {
            let mut archive = tar_archive(path, kind)?;
            let mut entries = Vec::new();
            let mut total = 0;
            let mut capped = false;
            for member in archive.entries().map_err(|e| arch_err(path, e))? {
                let member = match member {
                    Ok(member) => member,
                    Err(e) if is_scan_cap(&e) => {
                        capped = true;
                        break;
                    }
                    Err(e) => return Err(arch_err(path, e)),
                };
                total += 1;
                if entries.len() < MAX_ENTRIES {
                    entries.push(Entry {
                        name: member
                            .path()
                            .map_err(|e| arch_err(path, e))?
                            .display()
                            .to_string(),
                        size: member.size(),
                        is_dir: member.header().entry_type().is_dir(),
                    });
                }
            }
            (entries, total, capped)
        }
    };
    Ok(render(path, kind, entries, total, capped))
}

/// Extract one member's bytes (bounded). The name must match a listed
/// entry exactly; a miss says so rather than guessing.
pub(crate) fn read_entry(
    path: &Path,
    kind: Kind,
    entry: &str,
) -> Result<(Vec<u8>, bool), ToolError> {
    if entry.is_empty() {
        return Err(ToolError::InvalidInput(
            "`entry` must name an archive member — read the archive without \
             `entry` to list them"
                .into(),
        ));
    }
    let missing = || {
        ToolError::InvalidInput(format!(
            "no entry `{entry}` in {} — read the archive without `entry` to list them",
            path.display()
        ))
    };
    match kind {
        Kind::Gz => Err(ToolError::InvalidInput(format!(
            "{} is a plain gzip stream, not an archive — read it without \
             `entry` to see its decompressed content",
            path.display()
        ))),
        Kind::Zip => {
            let mut archive = zip_archive(path)?;
            let member = match archive.by_name(entry) {
                Ok(member) => member,
                Err(zip::result::ZipError::FileNotFound) => return Err(missing()),
                Err(e) => return Err(arch_err(path, e)),
            };
            bounded_read(member).map_err(|e| arch_err(path, e))
        }
        Kind::Tar | Kind::TarGz => {
            let scan_out = || {
                ToolError::Failed(format!(
                    "the {} MiB scan budget ran out before `{entry}` in {} — \
                     extract oversized archives with bash instead",
                    MAX_SCAN_BYTES / (1024 * 1024),
                    path.display()
                ))
            };
            let mut archive = tar_archive(path, kind)?;
            for member in archive.entries().map_err(|e| arch_err(path, e))? {
                let member = match member {
                    Ok(member) => member,
                    Err(e) if is_scan_cap(&e) => return Err(scan_out()),
                    Err(e) => return Err(arch_err(path, e)),
                };
                if member
                    .path()
                    .map_err(|e| arch_err(path, e))?
                    .display()
                    .to_string()
                    == entry
                {
                    return bounded_read(member).map_err(|e| {
                        if is_scan_cap(&e) {
                            scan_out()
                        } else {
                            arch_err(path, e)
                        }
                    });
                }
            }
            Err(missing())
        }
    }
}

/// A plain gzip stream: its decompressed content, bounded like any entry.
pub(crate) fn read_gz(path: &Path) -> Result<(Vec<u8>, bool), ToolError> {
    let file = std::fs::File::open(path).map_err(|e| arch_err(path, e))?;
    bounded_read(flate2::read::GzDecoder::new(file)).map_err(|e| arch_err(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("a.zip");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.add_directory("docs/", options).unwrap();
        writer.start_file("docs/readme.md", options).unwrap();
        writer.write_all(b"# hello\nworld\n").unwrap();
        writer.start_file("main.rs", options).unwrap();
        writer.write_all(b"fn main() {}\n").unwrap();
        writer.finish().unwrap();
        path
    }

    fn make_tar_gz(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("a.tgz");
        let file = std::fs::File::create(&path).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let data = b"key = value\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "conf/app.toml", &data[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn detects_by_magic_not_extension() {
        let dir = tempfile::tempdir().unwrap();
        let zip = make_zip(&dir);
        let tgz = make_tar_gz(&dir);
        // Deliberately misleading names.
        let disguised = dir.path().join("archive.txt");
        std::fs::copy(&zip, &disguised).unwrap();

        assert_eq!(detect(&zip), Some(Kind::Zip));
        assert_eq!(detect(&tgz), Some(Kind::TarGz));
        assert_eq!(detect(&disguised), Some(Kind::Zip));
        let plain = dir.path().join("notes.zip");
        std::fs::write(&plain, "not an archive").unwrap();
        assert_eq!(detect(&plain), None);
    }

    #[test]
    fn plain_tar_is_detected_by_ustar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar");
        let mut builder = tar::Builder::new(std::fs::File::create(&path).unwrap());
        let data = b"x";
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "one.txt", &data[..])
            .unwrap();
        builder.finish().unwrap();

        assert_eq!(detect(&path), Some(Kind::Tar));
        let out = list(&path, Kind::Tar).unwrap();
        assert!(out.contains("one.txt  1 bytes"), "{out}");
    }

    #[test]
    fn listings_show_entries_sizes_and_the_read_hint() {
        let dir = tempfile::tempdir().unwrap();
        let out = list(&make_zip(&dir), Kind::Zip).unwrap();
        assert!(out.contains("zip archive"), "{out}");
        assert!(out.contains("3 entries"), "{out}");
        assert!(out.contains("docs/\n"), "{out}");
        assert!(out.contains("docs/readme.md  14 bytes"), "{out}");
        assert!(out.contains("pass `entry`"), "{out}");

        let out = list(&make_tar_gz(&dir), Kind::TarGz).unwrap();
        assert!(out.contains("tar.gz archive"), "{out}");
        assert!(out.contains("conf/app.toml  12 bytes"), "{out}");
    }

    #[test]
    fn entries_extract_bounded_and_misses_name_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let zip = make_zip(&dir);
        let (bytes, clipped) = read_entry(&zip, Kind::Zip, "docs/readme.md").unwrap();
        assert_eq!(bytes, b"# hello\nworld\n");
        assert!(!clipped);

        let (bytes, _) = read_entry(&make_tar_gz(&dir), Kind::TarGz, "conf/app.toml").unwrap();
        assert_eq!(bytes, b"key = value\n");

        let err = read_entry(&zip, Kind::Zip, "nope.txt").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
        assert!(err.to_string().contains("without `entry`"), "{err}");
    }

    #[test]
    fn listings_cap_what_they_keep_but_count_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.tar");
        let mut builder = tar::Builder::new(std::fs::File::create(&path).unwrap());
        // Long names, so the rendered listing would blow past the output
        // cap if it were unbounded.
        let name = |i: usize| format!("{}{i:04}.txt", "n".repeat(150));
        for i in 0..(MAX_ENTRIES + 5) {
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name(i), &b"x"[..])
                .unwrap();
        }
        builder.finish().unwrap();

        let out = list(&path, Kind::Tar).unwrap();
        assert!(
            out.contains(&format!("({} entries)", MAX_ENTRIES + 5)),
            "{out}"
        );
        assert!(
            out.contains(&format!("[listed the first {MAX_ENTRIES}]")),
            "{out}"
        );
        assert!(!out.contains(&name(MAX_ENTRIES + 2)), "{out}");
        // The rendered output itself is bounded, not just the entry count.
        assert!(out.len() <= MAX_OUTPUT_BYTES + 200, "{}", out.len());
    }

    #[test]
    fn plain_gzip_text_is_gz_not_a_broken_tar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        gz.write_all(b"hello\nworld\n").unwrap();
        gz.finish().unwrap();

        assert_eq!(detect(&path), Some(Kind::Gz));
        let (bytes, clipped) = read_gz(&path).unwrap();
        assert_eq!(bytes, b"hello\nworld\n");
        assert!(!clipped);
        // The archive views refuse it with a pointer to the text path.
        assert!(matches!(
            list(&path, Kind::Gz),
            Err(ToolError::InvalidInput(_))
        ));
        assert!(matches!(
            read_entry(&path, Kind::Gz, "x"),
            Err(ToolError::InvalidInput(_))
        ));
    }

    #[test]
    fn tar_scans_stop_at_the_decompressed_byte_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomb.tgz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(gz);
        // First member decompresses past the scan budget; a marker hides
        // behind it. Compressed, the whole thing stays tiny.
        let big = MAX_SCAN_BYTES + 8 * 1024 * 1024;
        let mut header = tar::Header::new_gnu();
        header.set_size(big);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "large.bin", std::io::repeat(0).take(big))
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "marker.txt", &b"x"[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let out = list(&path, Kind::TarGz).unwrap();
        assert!(out.contains("1+ entries"), "{out}");
        assert!(out.contains("large.bin"), "{out}");
        assert!(out.contains("scan capped"), "{out}");
        assert!(!out.contains("marker.txt"), "{out}");

        let err = read_entry(&path, Kind::TarGz, "marker.txt").unwrap_err();
        assert!(err.to_string().contains("scan budget"), "{err}");
    }

    #[test]
    fn an_empty_entry_name_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_entry(&make_zip(&dir), Kind::Zip, "").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
    }

    #[test]
    fn extraction_is_capped_while_streaming() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        writer
            .start_file("big.bin", zip::write::SimpleFileOptions::default())
            .unwrap();
        // Highly compressible: a small archive that expands well past the cap.
        writer.write_all(&vec![b'a'; MAX_ENTRY_BYTES * 4]).unwrap();
        writer.finish().unwrap();

        let (bytes, clipped) = read_entry(&path, Kind::Zip, "big.bin").unwrap();
        assert_eq!(bytes.len(), MAX_ENTRY_BYTES);
        assert!(clipped);
    }
}
