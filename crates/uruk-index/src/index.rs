//! An index: the segments on disk, opened together.
//!
//! A query runs against every segment and the results are merged, so a
//! document is addressed by which segment it is in and its id within that
//! segment — see [`DocRef`]. Segment-local ids are what make delta encoding
//! work, and renumbering them globally at build time would mean rewriting
//! every segment whenever one was added, which is precisely what segments
//! exist to avoid.
//!
//! Corpus-wide statistics are the one thing that genuinely has to be global.
//! IDF asks "how rare is this term across everything?" and BM25 asks "how long
//! is a document, compared to the average?", so both are summed across
//! segments here rather than computed per segment, which would score the same
//! document differently depending on which file it happened to land in.

use std::path::{Path, PathBuf};

use crate::build::{IndexError, IndexManifest, read_manifest};
use crate::fields::Field;
use crate::postings::{DocPosting, Posting, PostingList};
use crate::segment::{DocEntry, SegmentReader};

/// Where a document lives: which segment, and its id inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocRef {
    pub segment: u16,
    pub doc: u32,
}

/// One term's postings within one segment.
#[derive(Debug)]
pub struct SegmentPostings {
    pub segment: u16,
    pub postings: Vec<Posting>,
}

/// Several segments, opened together and queried as one.
#[derive(Debug)]
pub struct Index {
    segments: Vec<SegmentReader>,
    manifest: IndexManifest,
    dir: PathBuf,
    /// Summed across segments, so scores do not depend on which file a
    /// document landed in.
    total_docs: u32,
    average_lengths: [f64; crate::fields::FIELD_COUNT],
}

impl Index {
    /// Open every segment named by the index manifest.
    pub fn open(dir: &Path) -> Result<Self, IndexError> {
        let manifest = read_manifest(dir)?;
        let mut segments = Vec::with_capacity(manifest.segments.len());
        for name in &manifest.segments {
            segments.push(SegmentReader::open(&dir.join(name))?);
        }

        let total_docs: u32 = segments
            .iter()
            .map(|segment| u32::try_from(segment.len()).unwrap_or(u32::MAX))
            .sum();

        // A weighted mean, not a mean of means: a segment holding ten
        // documents must not count as much as one holding fifty thousand.
        let mut average_lengths = [0.0; crate::fields::FIELD_COUNT];
        if total_docs > 0 {
            for field in Field::ALL {
                let total: f64 = segments
                    .iter()
                    .map(|segment| segment.average_length(field) * segment.len() as f64)
                    .sum();
                average_lengths[field.index()] = total / f64::from(total_docs);
            }
        }

        Ok(Self {
            segments,
            manifest,
            dir: dir.to_path_buf(),
            total_docs,
            average_lengths,
        })
    }

    pub fn manifest(&self) -> &IndexManifest {
        &self.manifest
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Documents across every segment. The `N` in IDF.
    pub fn len(&self) -> u32 {
        self.total_docs
    }

    pub fn is_empty(&self) -> bool {
        self.total_docs == 0
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Average tokens per document in a field, across the whole index.
    pub fn average_length(&self, field: Field) -> f64 {
        self.average_lengths[field.index()]
    }

    /// How many documents in the whole index contain `term`.
    ///
    /// Read from the dictionaries, so this costs no posting-list reads — which
    /// matters because a query asks it for every term before deciding what to
    /// do.
    pub fn doc_frequency(&self, term: &str) -> u32 {
        self.segments
            .iter()
            .map(|segment| segment.doc_frequency(term))
            .sum()
    }

    /// Read one term's postings from every segment that has any.
    pub fn postings(&mut self, term: &str) -> Result<Vec<SegmentPostings>, IndexError> {
        let mut out = Vec::new();
        for (position, segment) in self.segments.iter_mut().enumerate() {
            // Skip segments the dictionary says cannot contribute, so a rare
            // term costs one binary search per segment and no reads.
            if segment.doc_frequency(term) == 0 {
                continue;
            }
            let postings = segment.postings(term)?;
            if !postings.is_empty() {
                out.push(SegmentPostings {
                    segment: u16::try_from(position).unwrap_or(u16::MAX),
                    postings,
                });
            }
        }
        Ok(out)
    }

    /// A document's lengths and its place in the crawl store.
    pub fn doc(&self, reference: DocRef) -> Option<DocEntry> {
        self.segments
            .get(reference.segment as usize)?
            .doc(reference.doc)
    }

    /// One term's postings from one segment.
    ///
    /// A query walks segments itself rather than taking [`Self::postings`],
    /// because it also needs to filter and score per segment and would
    /// otherwise hold every segment's lists at once.
    pub fn segment_postings(
        &mut self,
        segment: u16,
        term: &str,
    ) -> Result<Vec<Posting>, IndexError> {
        let Some(reader) = self.segments.get_mut(segment as usize) else {
            return Ok(Vec::new());
        };
        // The dictionary answers this without a read, so a term absent from
        // this segment costs a binary search rather than a seek.
        if reader.doc_frequency(term) == 0 {
            return Ok(Vec::new());
        }
        Ok(reader.postings(term)?)
    }

    /// One term's postings from one segment, with positions readable on
    /// demand rather than decoded up front.
    pub fn segment_posting_list(
        &mut self,
        segment: u16,
        term: &str,
    ) -> Result<PostingList, IndexError> {
        let Some(reader) = self.segments.get_mut(segment as usize) else {
            return Ok(PostingList::default());
        };
        if reader.doc_frequency(term) == 0 {
            return Ok(PostingList::default());
        }
        Ok(reader.posting_list(term)?)
    }

    /// One term's postings from one segment, without decoding positions.
    ///
    /// For queries that cannot use them — a single term, or an excluded one.
    /// Positions are the largest part of the index and the slowest part to
    /// decode, so this is most of the difference between a query inside the
    /// brief's latency budget and one outside it.
    pub fn segment_document_postings(
        &mut self,
        segment: u16,
        term: &str,
    ) -> Result<Vec<DocPosting>, IndexError> {
        let Some(reader) = self.segments.get_mut(segment as usize) else {
            return Ok(Vec::new());
        };
        if reader.doc_frequency(term) == 0 {
            return Ok(Vec::new());
        }
        Ok(reader.document_postings(term)?)
    }

    /// The host id a `site:` filter should match within one segment.
    ///
    /// `None` means the segment holds nothing from that host and can be
    /// skipped whole, before any posting list is touched.
    pub fn host_id(&self, segment: u16, host: &str) -> Option<u32> {
        self.segments.get(segment as usize)?.host_id(host)
    }

    /// A host's name within one segment, by the id the document table holds.
    ///
    /// Separate from [`Self::host_name`] because scoring wants to look a host
    /// up once and reuse it: many documents share a host, and resolving the
    /// name through a document reference each time turns a per-host cost into
    /// a per-candidate one.
    pub fn host_name_of(&self, segment: u16, host: u32) -> Option<&str> {
        self.segments.get(segment as usize)?.host_name(host)
    }

    /// A document's host name, for display.
    pub fn host_name(&self, reference: DocRef) -> Option<&str> {
        let reader = self.segments.get(reference.segment as usize)?;
        reader.host_name(reader.doc(reference.doc)?.host)
    }
}

#[cfg(test)]
mod tests {
    use super::{DocRef, Index};
    use crate::build::{IndexConfig, build};
    use crate::fields::Field;
    use uruk_crawl::store::{CrawlSummary, QualitySignals, Record, StoreWriter};

    fn record(n: usize, title: &str, text: &str) -> Record {
        Record {
            url: format!("https://a.test/p/{n}"),
            final_url: format!("https://a.test/p/{n}"),
            fetched_at: 0,
            status: 200,
            depth: 1,
            fingerprint: n as u64,
            title: title.to_owned(),
            text: text.to_owned(),
            headings: vec![],
            lang: Some("en".into()),
            links: vec![],
            quality: QualitySignals {
                text_ratio: 0.5,
                link_density: 0.0,
                scripts: 0,
                words: text.split_whitespace().count(),
            },
            snippet_allowed: true,
            max_snippet: None,
        }
    }

    /// Three documents across two segments, so cross-segment merging is
    /// exercised rather than assumed.
    fn two_segment_index(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("uruk-index-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let crawl = root.join("crawl");
        let index = root.join("index");

        let mut writer = StoreWriter::create(&crawl).unwrap();
        writer
            .push(&record(0, "Clay tablets", "uruk clay reed barley"))
            .unwrap();
        writer
            .push(&record(1, "Harbours", "sediment silt dredging channel"))
            .unwrap();
        writer
            .push(&record(2, "More on Uruk", "uruk again clay and more clay"))
            .unwrap();
        writer.finish(&CrawlSummary::default()).unwrap();

        build(&IndexConfig {
            crawl_dir: crawl,
            out_dir: index.clone(),
            docs_per_segment: 2,
            progress: false,
        })
        .unwrap();
        index
    }

    #[test]
    fn opens_every_segment_the_manifest_names() {
        let dir = two_segment_index("open");
        let index = Index::open(&dir).unwrap();
        assert_eq!(index.segment_count(), 2);
        assert_eq!(index.len(), 3);
        assert!(!index.is_empty());
    }

    #[test]
    fn document_frequency_is_summed_across_segments() {
        // "uruk" is in documents 0 and 2, which are in different segments.
        // Scoring one of them differently because of that would be a bug.
        let dir = two_segment_index("df");
        let index = Index::open(&dir).unwrap();
        assert_eq!(index.doc_frequency("uruk"), 2);
        assert_eq!(index.doc_frequency("sediment"), 1);
        assert_eq!(index.doc_frequency("babylon"), 0);
    }

    #[test]
    fn postings_come_back_from_every_segment_that_has_them() {
        let dir = two_segment_index("postings");
        let mut index = Index::open(&dir).unwrap();

        let hits = index.postings("uruk").unwrap();
        assert_eq!(hits.len(), 2, "one entry per contributing segment");
        assert_eq!(hits[0].segment, 0);
        assert_eq!(hits[1].segment, 1);
        assert_eq!(hits[1].postings[0].doc, 0, "ids are segment-local");
    }

    #[test]
    fn a_term_in_one_segment_only_reads_that_segment() {
        let dir = two_segment_index("single-seg");
        let mut index = Index::open(&dir).unwrap();
        let hits = index.postings("sediment").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].segment, 0);
    }

    #[test]
    fn an_absent_term_reads_nothing() {
        let dir = two_segment_index("absent");
        let mut index = Index::open(&dir).unwrap();
        assert!(index.postings("babylon").unwrap().is_empty());
    }

    #[test]
    fn average_length_is_weighted_by_segment_size() {
        // A mean of means would let a two-document segment outvote a
        // fifty-thousand-document one.
        let dir = two_segment_index("avgdl");
        let index = Index::open(&dir).unwrap();

        // Bodies are 4, 4 and 6 tokens: the mean is 14/3.
        let expected = 14.0 / 3.0;
        assert!(
            (index.average_length(Field::Body) - expected).abs() < 1e-9,
            "got {}",
            index.average_length(Field::Body)
        );
    }

    #[test]
    fn documents_resolve_back_to_the_crawl_store() {
        let dir = two_segment_index("docref");
        let index = Index::open(&dir).unwrap();

        // Segment 1, document 0 is the third crawled page.
        let entry = index.doc(DocRef { segment: 1, doc: 0 }).unwrap();
        assert_eq!(entry.crawl_doc, 2);
        assert!(index.doc(DocRef { segment: 9, doc: 0 }).is_none());
        assert!(
            index
                .doc(DocRef {
                    segment: 0,
                    doc: 99
                })
                .is_none()
        );
    }

    #[test]
    fn an_empty_index_opens_without_dividing_by_zero() {
        let root = std::env::temp_dir().join(format!("uruk-index-{}-empty", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let crawl = root.join("crawl");
        StoreWriter::create(&crawl)
            .unwrap()
            .finish(&CrawlSummary::default())
            .unwrap();
        build(&IndexConfig {
            crawl_dir: crawl,
            out_dir: root.join("index"),
            docs_per_segment: 10,
            progress: false,
        })
        .unwrap();

        let index = Index::open(&root.join("index")).unwrap();
        assert!(index.is_empty());
        assert!(index.average_length(Field::Body).abs() < f64::EPSILON);
        assert_eq!(index.doc_frequency("anything"), 0);
    }
}
