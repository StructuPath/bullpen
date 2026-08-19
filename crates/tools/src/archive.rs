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

use crate::ToolError;

/// Listing cap; the true total is always reported.
const MAX_ENTRIES: usize = 1_000;
/// Extraction cap, enforced while streaming (zip bombs stay in the bottle).
pub(crate) const MAX_ENTRY_BYTES: usize = 262_144; // matches file reads

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Zip,
    Tar,
    TarGz,
}

/// Recognize an archive by content: zip's `PK` local/empty headers, gzip's
/// two magic bytes, or tar's `ustar` at offset 257.
pub(crate) fn detect(path: &Path) -> Option<Kind> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 265];
    let n = file.read(&mut head).ok()?;
    let head = &head[..n];
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Some(Kind::Zip);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return Some(Kind::TarGz);
    }
    if n >= 262 && &head[257..262] == b"ustar" {
        return Some(Kind::Tar);
    }
    None
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

fn render(path: &Path, kind: Kind, entries: Vec<Entry>) -> String {
    let label = match kind {
        Kind::Zip => "zip",
        Kind::Tar => "tar",
        Kind::TarGz => "tar.gz",
    };
    if entries.is_empty() {
        return format!("(empty {label} archive {})", path.display());
    }
    let total = entries.len();
    let mut out = format!(
        "{label} archive {} ({total} entries) — pass `entry` to read one:\n",
        path.display()
    );
    for entry in entries.iter().take(MAX_ENTRIES) {
        if entry.is_dir {
            out.push_str(&format!("  {}\n", entry.name));
        } else {
            out.push_str(&format!("  {}  {} bytes\n", entry.name, entry.size));
        }
    }
    if total > MAX_ENTRIES {
        out.push_str(&format!("[listed the first {MAX_ENTRIES}]\n"));
    }
    out
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
    let reader: Box<dyn Read> = match kind {
        Kind::TarGz => Box::new(flate2::read::GzDecoder::new(file)),
        _ => Box::new(file),
    };
    Ok(tar::Archive::new(reader))
}

/// The listing view.
pub(crate) fn list(path: &Path, kind: Kind) -> Result<String, ToolError> {
    let entries = match kind {
        Kind::Zip => {
            let mut archive = zip_archive(path)?;
            let mut entries = Vec::new();
            for i in 0..archive.len() {
                let member = archive.by_index(i).map_err(|e| arch_err(path, e))?;
                entries.push(Entry {
                    name: member.name().to_string(),
                    size: member.size(),
                    is_dir: member.is_dir(),
                });
            }
            entries
        }
        Kind::Tar | Kind::TarGz => {
            let mut archive = tar_archive(path, kind)?;
            let mut entries = Vec::new();
            for member in archive.entries().map_err(|e| arch_err(path, e))? {
                let member = member.map_err(|e| arch_err(path, e))?;
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
            entries
        }
    };
    Ok(render(path, kind, entries))
}

/// Extract one member's bytes (bounded). The name must match a listed
/// entry exactly; a miss says so rather than guessing.
pub(crate) fn read_entry(
    path: &Path,
    kind: Kind,
    entry: &str,
) -> Result<(Vec<u8>, bool), ToolError> {
    let missing = || {
        ToolError::InvalidInput(format!(
            "no entry `{entry}` in {} — read the archive without `entry` to list them",
            path.display()
        ))
    };
    match kind {
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
            let mut archive = tar_archive(path, kind)?;
            for member in archive.entries().map_err(|e| arch_err(path, e))? {
                let member = member.map_err(|e| arch_err(path, e))?;
                if member
                    .path()
                    .map_err(|e| arch_err(path, e))?
                    .display()
                    .to_string()
                    == entry
                {
                    return bounded_read(member).map_err(|e| arch_err(path, e));
                }
            }
            Err(missing())
        }
    }
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
