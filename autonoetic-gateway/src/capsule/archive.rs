//! tar.zst archive helpers for capsules.
//!
//! Capsules are packed as `tar.zst` archives with this directory layout
//! (see design doc, section "Capsule Archive Structure"):
//!
//! ```text
//! capsule_<id>/
//! ├── capsule.json
//! ├── agent/
//! │   ├── SKILL.md
//! │   ├── runtime.lock
//! │   └── files/
//! ├── artifacts/   (hermetic only)
//! ├── layers/      (hermetic only)
//! ├── memory/      (opt-in)
//! └── checkpoint/  (replay only)
//! ```
//!
//! Pack takes a staging directory + an output path; unpack takes an
//! archive path + a target directory. Importers MUST provide an extracted
//! byte-size cap so a malicious archive cannot fill the filesystem.

use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use tar::{Archive as TarArchive, Builder as TarBuilder};
use zstd::{Decoder as ZstdDecoder, Encoder as ZstdEncoder};

const ZSTD_LEVEL: i32 = 3;

/// Pack `source_dir` as a tar.zst archive at `output_path`. Existing
/// files at `output_path` are overwritten.
pub fn pack(source_dir: &Path, output_path: &Path) -> Result<()> {
    let f = File::create(output_path)
        .with_context(|| format!("creating capsule archive at {}", output_path.display()))?;
    let writer = BufWriter::new(f);
    let encoder = ZstdEncoder::new(writer, ZSTD_LEVEL)?;
    let mut tar_builder = TarBuilder::new(encoder);
    tar_builder
        .append_dir_all(".", source_dir)
        .with_context(|| format!("tarring {}", source_dir.display()))?;
    let encoder = tar_builder.into_inner()?;
    encoder.finish()?;
    Ok(())
}

/// Unpack a tar.zst archive into `target_dir`.
///
/// Refuses if the cumulative uncompressed size exceeds `max_extract_bytes`
/// (resource-exhaustion guard). Refuses path traversal (entries whose tar
/// header path escapes `target_dir`).
pub fn unpack(archive_path: &Path, target_dir: &Path, max_extract_bytes: u64) -> Result<u64> {
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("creating extract dir {}", target_dir.display()))?;
    let file = File::open(archive_path)
        .with_context(|| format!("opening capsule archive {}", archive_path.display()))?;
    let reader = BufReader::new(file);
    let decoder = ZstdDecoder::new(reader)?;
    let mut archive = TarArchive::new(decoder);
    archive.set_preserve_permissions(false);
    archive.set_overwrite(true);

    let target_canonical = target_dir
        .canonicalize()
        .with_context(|| format!("canonicalising target {}", target_dir.display()))?;

    let mut total: u64 = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let header_size = entry.header().size().unwrap_or(0);
        total = total
            .checked_add(header_size)
            .ok_or_else(|| anyhow::anyhow!("capsule size overflow"))?;
        if total > max_extract_bytes {
            anyhow::bail!(
                "capsule extraction exceeds max size {} bytes",
                max_extract_bytes
            );
        }
        // Defend against path traversal: reject absolute paths or any
        // `..` segment. Components like `.` and the bare archive root
        // (`./`) are harmless and pass through.
        let path = entry.path()?.into_owned();
        for component in path.components() {
            use std::path::Component;
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    anyhow::bail!(
                        "capsule entry has absolute path: {}",
                        path.display()
                    );
                }
                Component::ParentDir => {
                    anyhow::bail!(
                        "capsule entry contains parent-dir segment: {}",
                        path.display()
                    );
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }
        entry.unpack_in(&target_canonical)?;
    }
    Ok(total)
}

/// Read the raw bytes of a file within an extracted capsule directory.
pub fn read_entry(capsule_root: &Path, relative: &str) -> Result<Vec<u8>> {
    let mut f = File::open(capsule_root.join(relative))
        .with_context(|| format!("opening capsule entry {}", relative))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Write bytes to a file within a staging capsule directory, creating
/// parent directories as needed.
pub fn write_entry(staging_root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    let p = staging_root.join(relative);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = File::create(&p)
        .with_context(|| format!("creating capsule entry {}", p.display()))?;
    let mut w = BufWriter::new(f);
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn pack_then_unpack_roundtrips_files() {
        let src = tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("agent")).unwrap();
        std::fs::write(src.path().join("agent/SKILL.md"), b"# hi\n").unwrap();
        std::fs::write(src.path().join("capsule.json"), b"{}").unwrap();

        let out = tempdir().unwrap();
        let archive = out.path().join("c.tar.zst");
        pack(src.path(), &archive).unwrap();
        assert!(archive.exists());

        let extract = tempdir().unwrap();
        let total = unpack(&archive, extract.path(), 64 * 1024).unwrap();
        assert!(total > 0);
        let skill = std::fs::read(extract.path().join("agent/SKILL.md")).unwrap();
        assert_eq!(skill, b"# hi\n");
        let manifest = std::fs::read(extract.path().join("capsule.json")).unwrap();
        assert_eq!(manifest, b"{}");
    }

    #[test]
    fn unpack_refuses_when_total_exceeds_cap() {
        let src = tempdir().unwrap();
        std::fs::write(src.path().join("big.bin"), vec![0u8; 4096]).unwrap();
        let out = tempdir().unwrap();
        let archive = out.path().join("c.tar.zst");
        pack(src.path(), &archive).unwrap();
        let extract = tempdir().unwrap();
        let err = unpack(&archive, extract.path(), 1024).expect_err("cap should trip");
        assert!(err.to_string().contains("max size"), "{err}");
    }

    #[test]
    fn write_then_read_entry_roundtrip() {
        let staging = tempdir().unwrap();
        write_entry(staging.path(), "a/b/c.txt", b"hello").unwrap();
        let bytes = read_entry(staging.path(), "a/b/c.txt").unwrap();
        assert_eq!(bytes, b"hello");
    }
}
