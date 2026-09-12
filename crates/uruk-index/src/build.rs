//! Building an index from a crawl.
//!
//! Reads the crawl store a page at a time and writes segments. Documents are
//! flushed into a new segment every [`IndexConfig::docs_per_segment`], which
//! is what keeps peak memory bounded by the segment size rather than by the
//! size of the crawl: a builder holds one segment's postings, writes them, and
//! starts empty again.
//!
//! That is also the shape the brief asks for (`RESEARCH.md` §2.3). New crawl
//! data makes new segments; nothing rewrites the ones already on disk.
//! Background merging of small segments into large ones is the next step and
//! is deliberately not here yet — merging matters once an index is updated
//! repeatedly, and this one is still built in a single pass.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uruk_crawl::store::{StoreError, StoreReader};

use crate::segment::{DocQuality, Document, SegmentBuilder, SegmentError, SegmentManifest};

/// Documents per segment.
///
/// Chosen so one segment's postings sit comfortably in memory while building.
/// Smaller means more files and more work at query time; larger means a bigger
/// peak while indexing.
pub const DEFAULT_DOCS_PER_SEGMENT: usize = 50_000;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("could not read the crawl store: {0}")]
    Store(#[from] StoreError),
    #[error("could not write the segment: {0}")]
    Segment(#[from] SegmentError),
    #[error("index I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not write the index manifest: {0}")]
    Json(#[from] serde_json::Error),
}

/// How to build an index.
#[derive(Debug, Clone)]
pub struct IndexConfig {
    /// Directory holding a crawl store.
    pub crawl_dir: PathBuf,
    /// Directory to write segments into.
    pub out_dir: PathBuf,
    pub docs_per_segment: usize,
    pub progress: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            crawl_dir: PathBuf::from("data/crawl"),
            out_dir: PathBuf::from("data/index"),
            docs_per_segment: DEFAULT_DOCS_PER_SEGMENT,
            progress: true,
        }
    }
}

/// What an index contains, written to `index.json` beside the segments.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexManifest {
    /// Segment file names, in build order.
    pub segments: Vec<String>,
    pub documents: u32,
    pub terms: u64,
    pub postings: u64,
    pub bytes_total: u64,
    pub bytes_postings: u64,
    pub bytes_dictionary: u64,
    #[serde(default)]
    pub bytes_hosts: u64,
    pub bytes_doc_table: u64,
    /// Bytes of extracted text indexed, so the size of the index can be quoted
    /// as a fraction of it — the number `RESEARCH.md` §6 argues about.
    pub text_bytes: u64,
    /// Documents skipped because they had no indexable text.
    pub skipped_empty: u32,
}

impl IndexManifest {
    /// Index size as a percentage of the text it describes.
    ///
    /// The brief's target was 15–25%; `RESEARCH.md` §6 restated that as
    /// achievable without positions and optimistic with them. This is how the
    /// claim gets checked against a real corpus rather than argued about.
    pub fn size_ratio(&self) -> f64 {
        if self.text_bytes == 0 {
            return 0.0;
        }
        self.bytes_total as f64 / self.text_bytes as f64
    }

    fn absorb(&mut self, segment: &SegmentManifest, name: String) {
        self.segments.push(name);
        self.documents += segment.documents;
        self.terms += segment.terms;
        self.postings += segment.postings;
        self.bytes_total += segment.bytes_total;
        self.bytes_postings += segment.bytes_postings;
        self.bytes_dictionary += segment.bytes_dictionary;
        self.bytes_hosts += segment.bytes_hosts;
        self.bytes_doc_table += segment.bytes_doc_table;
    }
}

/// Build an index from a crawl store.
pub fn build(config: &IndexConfig) -> Result<IndexManifest, IndexError> {
    let mut store = StoreReader::open(&config.crawl_dir)?;
    std::fs::create_dir_all(&config.out_dir)?;

    let mut manifest = IndexManifest::default();
    let mut builder = SegmentBuilder::new();

    // `records()` walks the store one compressed block at a time rather than
    // holding the corpus, so a crawl larger than memory still indexes.
    // The position in that walk *is* the crawl store's document id, which is
    // how a result finds its text again.
    for (position, record) in store.records()?.enumerate() {
        let record = record?;
        let id = u32::try_from(position).expect("a crawl holds fewer than 4 billion pages");

        // A document with no text contributes nothing but a doc-table entry
        // and a length of zero, which would skew BM25's average length.
        if record.text.trim().is_empty() {
            manifest.skipped_empty += 1;
            continue;
        }

        manifest.text_bytes += record.text.len() as u64;
        builder.add(&Document {
            crawl_doc: id,
            url: &record.url,
            title: &record.title,
            headings: &record.headings,
            body: &record.text,
            host: host_of(&record.url),
            quality: DocQuality {
                #[expect(clippy::cast_possible_truncation, reason = "f64 to f32 for storage")]
                text_ratio: record.quality.text_ratio as f32,
                #[expect(clippy::cast_possible_truncation, reason = "f64 to f32 for storage")]
                link_density: record.quality.link_density as f32,
                scripts: u32::try_from(record.quality.scripts).unwrap_or(u32::MAX),
            },
        });

        if builder.len() >= config.docs_per_segment {
            flush(
                &mut builder,
                &config.out_dir,
                &mut manifest,
                config.progress,
            )?;
        }
    }
    if !builder.is_empty() {
        flush(
            &mut builder,
            &config.out_dir,
            &mut manifest,
            config.progress,
        )?;
    }

    std::fs::write(
        config.out_dir.join("index.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

/// Write the current builder out as a segment and start a fresh one.
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
            "uruk-index: wrote {name}: {} documents, {} terms, {} bytes",
            written.documents, written.terms, written.bytes_total
        );
    }
    manifest.absorb(&written, name);
    *builder = SegmentBuilder::new();
    Ok(())
}

/// The host part of a URL, for `site:` filtering.
///
/// Deliberately a string slice rather than a parse: the crawler already
/// normalised and validated these URLs, so a second full parse per document
/// would be paid for nothing.
fn host_of(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Drop any userinfo and port.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    authority
        .split_once(':')
        .map_or(authority, |(host, _)| host)
}

/// Read an index's manifest without opening its segments.
pub fn read_manifest(dir: &Path) -> Result<IndexManifest, IndexError> {
    let raw = std::fs::read_to_string(dir.join("index.json"))?;
    Ok(serde_json::from_str(&raw)?)
}

#[cfg(test)]
mod tests {
    use super::{IndexConfig, build, read_manifest};
    use uruk_crawl::store::{CrawlSummary, OutLink, QualitySignals, Record, StoreWriter};

    fn record(n: usize, text: &str) -> Record {
        Record {
            url: format!("https://a.test/page/{n}"),
            final_url: format!("https://a.test/page/{n}"),
            fetched_at: 1_700_000_000,
            status: 200,
            depth: 1,
            fingerprint: n as u64,
            title: format!("Page {n} about clay"),
            text: text.to_owned(),
            headings: vec![format!("Heading {n}")],
            lang: Some("en".into()),
            links: vec![OutLink {
                url: "https://a.test/next".into(),
                anchor: "next".into(),
                nofollow: false,
            }],
            quality: QualitySignals {
                text_ratio: 0.4,
                link_density: 0.1,
                scripts: 1,
                words: text.split_whitespace().count(),
            },
            snippet_allowed: true,
            max_snippet: None,
        }
    }

    /// Write a crawl store and index it. Returns (crawl dir, index dir).
    fn crawl_and_index(
        name: &str,
        docs: usize,
        docs_per_segment: usize,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("uruk-build-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let crawl_dir = root.join("crawl");
        let index_dir = root.join("index");

        let mut writer = StoreWriter::create(&crawl_dir).unwrap();
        for n in 0..docs {
            let text = format!(
                "document {n} the scribes of uruk pressed reed into clay tablets \
                 to record barley and sheep for the temple storehouse"
            );
            writer.push(&record(n, &text)).unwrap();
        }
        // One document with no text, to prove it is skipped.
        writer.push(&record(docs, "   ")).unwrap();
        writer.finish(&CrawlSummary::default()).unwrap();

        build(&IndexConfig {
            crawl_dir: crawl_dir.clone(),
            out_dir: index_dir.clone(),
            docs_per_segment,
            progress: false,
        })
        .unwrap();

        (crawl_dir, index_dir)
    }

    #[test]
    fn hosts_are_extracted_from_urls() {
        use super::host_of;
        assert_eq!(host_of("https://a.test/blog/post"), "a.test");
        assert_eq!(host_of("http://a.test:8080/p?x=1"), "a.test");
        assert_eq!(host_of("https://user@a.test/p"), "a.test");
        assert_eq!(host_of("https://a.test"), "a.test");
        assert_eq!(host_of("a.test/p"), "a.test");
    }

    #[test]
    fn indexes_a_crawl_into_one_segment() {
        let (_, index_dir) = crawl_and_index("single", 10, 1_000);
        let manifest = read_manifest(&index_dir).unwrap();

        assert_eq!(manifest.segments.len(), 1);
        assert_eq!(manifest.documents, 10);
        assert_eq!(manifest.skipped_empty, 1);
        assert!(manifest.terms > 10);
        assert!(manifest.postings >= manifest.terms);
        assert!(manifest.text_bytes > 0);
    }

    #[test]
    fn rolls_over_into_several_segments() {
        // The property that bounds memory: a big crawl becomes many segments.
        let (_, index_dir) = crawl_and_index("rollover", 25, 10);
        let manifest = read_manifest(&index_dir).unwrap();

        assert_eq!(manifest.segments.len(), 3, "25 docs at 10 per segment");
        assert_eq!(manifest.documents, 25);
        for name in &manifest.segments {
            assert!(index_dir.join(name).exists(), "{name} is missing");
        }
    }

    #[test]
    fn the_manifest_totals_match_the_files_on_disk() {
        let (_, index_dir) = crawl_and_index("totals", 25, 10);
        let manifest = read_manifest(&index_dir).unwrap();

        let on_disk: u64 = manifest
            .segments
            .iter()
            .map(|name| std::fs::metadata(index_dir.join(name)).unwrap().len())
            .sum();
        assert_eq!(manifest.bytes_total, on_disk);
    }

    #[test]
    fn the_size_ratio_is_reported() {
        // The number RESEARCH.md section 6 argues about; here it just has to
        // be computed from real bytes rather than asserted to a target.
        let (_, index_dir) = crawl_and_index("ratio", 50, 1_000);
        let manifest = read_manifest(&index_dir).unwrap();
        assert!(manifest.size_ratio() > 0.0);
        assert!(
            manifest.size_ratio() < 10.0,
            "ratio: {}",
            manifest.size_ratio()
        );
    }

    #[test]
    fn an_empty_crawl_produces_an_empty_index() {
        let root = std::env::temp_dir().join(format!("uruk-build-{}-empty", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let crawl_dir = root.join("crawl");
        StoreWriter::create(&crawl_dir)
            .unwrap()
            .finish(&CrawlSummary::default())
            .unwrap();

        let manifest = build(&IndexConfig {
            crawl_dir,
            out_dir: root.join("index"),
            docs_per_segment: 10,
            progress: false,
        })
        .unwrap();

        assert!(manifest.segments.is_empty());
        assert_eq!(manifest.documents, 0);
        assert!(manifest.size_ratio().abs() < f64::EPSILON);
    }

    #[test]
    fn a_missing_crawl_store_is_an_error_not_a_panic() {
        let result = build(&IndexConfig {
            crawl_dir: "/nonexistent/uruk/crawl".into(),
            out_dir: std::env::temp_dir().join("uruk-build-missing"),
            docs_per_segment: 10,
            progress: false,
        });
        assert!(result.is_err());
    }
}
