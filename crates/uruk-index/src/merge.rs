//! Combining segments, so a query reads fewer of them.
//!
//! An index is written as segments because building one holds a whole
//! segment's postings in memory, and a bounded segment is what bounds that.
//! But nothing ever combined them afterwards, and a query pays for that on
//! every search: a term's posting list is read **once per segment**, and each
//! segment's dictionary is searched separately. Twenty segments make a
//! three-word query twenty times as many reads as it needs to be.
//!
//! Merging is not rebuilding. It never touches the crawl store and never
//! re-tokenises anything: it reads decoded postings out of the old segments,
//! shifts their document ids, and writes them back out. What comes out has to
//! answer every query identically, and the test for it asserts exactly that
//! rather than comparing file bytes.
//!
//! # What it costs while it runs
//!
//! One output segment's postings are held in memory at once, the same as
//! building. That is the reason `docs_per_segment` exists here rather than
//! merging everything into one file unconditionally: on a million-document
//! index, "everything" is gigabytes.

use std::path::{Path, PathBuf};

use crate::build::{IndexError, IndexManifest, read_manifest};
use crate::segment::{SegmentBuilder, SegmentReader};

/// How to merge.
#[derive(Debug, Clone)]
pub struct MergeConfig {
    pub index_dir: PathBuf,
    /// Where the merged index is written. Deliberately a separate directory:
    /// merging in place would leave the index unreadable if it failed
    /// half-way, and an index is expensive enough to rebuild that the safe
    /// version is worth the disk.
    pub out_dir: PathBuf,
    /// Most documents in an output segment. Bounds peak memory.
    pub docs_per_segment: usize,
    pub progress: bool,
}

/// Merge every segment of an index into as few as the size bound allows.
pub fn merge(config: &MergeConfig) -> Result<IndexManifest, IndexError> {
    let source = read_manifest(&config.index_dir)?;
    std::fs::create_dir_all(&config.out_dir)?;

    let mut manifest = IndexManifest {
        text_bytes: source.text_bytes,
        skipped_empty: source.skipped_empty,
        ..IndexManifest::default()
    };

    let limit = config.docs_per_segment.max(1);
    let mut builder = SegmentBuilder::new();

    for name in &source.segments {
        let mut reader = SegmentReader::open(&config.index_dir.join(name))?;

        // Flush before absorbing, not after: a segment that would push the
        // builder past the limit starts a new one, so the bound holds even
        // when a single input segment is larger than it. (It cannot be
        // smaller than one input segment, and saying so is better than
        // pretending the bound is exact.)
        if !builder.is_empty() && builder.len() + reader.len() > limit {
            flush(
                &mut builder,
                &config.out_dir,
                &mut manifest,
                config.progress,
            )?;
        }

        if config.progress {
            eprintln!(
                "uruk-merge: absorbing {name}: {} documents, {} terms",
                reader.len(),
                reader.terms()
            );
        }
        builder.absorb(&mut reader)?;
    }

    if !builder.is_empty() {
        flush(
            &mut builder,
            &config.out_dir,
            &mut manifest,
            config.progress,
        )?;
    }

    let json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(config.out_dir.join("index.json"), json)?;
    Ok(manifest)
}

fn flush(
    builder: &mut SegmentBuilder,
    out_dir: &Path,
    manifest: &mut IndexManifest,
    progress: bool,
) -> Result<(), IndexError> {
    let name = format!("segment-{:05}.uruk", manifest.segments.len());
    let written = builder.write(&out_dir.join(&name))?;
    if progress {
        eprintln!(
            "uruk-merge: wrote {name}: {} documents, {} terms, {} bytes",
            written.documents, written.terms, written.bytes_total
        );
    }
    manifest.absorb(&written, name);
    *builder = SegmentBuilder::new();
    Ok(())
}
