//! The crawl store: extracted text and metadata, compressed on disk.
//!
//! Two files, plus a summary:
//!
//! - `pages.uruk` — the records, in **independently compressed blocks**.
//! - `pages.idx` — a fixed-width table saying which block each record is in.
//! - `crawl.json` — what happened during the crawl, for humans.
//!
//! Blocks rather than one compressed stream, for three reasons that all matter
//! later. Showing a snippet means reading one document, and a block boundary is
//! what stops that from decompressing the entire corpus — the rule in
//! `RESEARCH.md` §5.5 about touching only the blocks a query needs starts here.
//! An interrupted crawl loses at most the block being written, not the file.
//! And compressing a whole block at once still gets the ratio that
//! record-at-a-time compression would throw away.
//!
//! Raw HTML is deliberately never stored. Keeping it would multiply the corpus
//! several times over for something we only need in order to show 150
//! characters (`RESEARCH.md` §3.3).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Uncompressed bytes buffered before a block is flushed.
///
/// A trade-off between compression ratio (bigger is better) and the cost of
/// reading one document (smaller is better). 256 KiB of article text is a few
/// dozen documents, which is a cheap read for a snippet.
const BLOCK_TARGET: usize = 256 * 1024;

/// Zstandard level. 9 is well past the point of diminishing returns for speed
/// and well short of the point where compression time becomes the bottleneck.
const ZSTD_LEVEL: i32 = 9;

const PAGES_MAGIC: &[u8; 8] = b"URUKCRWL";
const INDEX_MAGIC: &[u8; 8] = b"URUKCIDX";
const FORMAT_VERSION: u32 = 1;

/// Bytes per index entry: an 8-byte block offset and a 4-byte record ordinal.
const INDEX_ENTRY: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not encode or decode a record: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{path} is not a uruk store (bad magic)")]
    BadMagic { path: PathBuf },
    #[error("{path} is format version {found}, but this build understands {expected}")]
    VersionMismatch {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
    #[error("no document {0} in this store")]
    NoSuchDocument(u32),
    #[error("store is corrupt: {0}")]
    Corrupt(String),
}

/// A link found on a crawled page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutLink {
    pub url: String,
    /// Anchor text, which Phase 4 weights as a field of the *target* page.
    pub anchor: String,
    pub nofollow: bool,
}

/// Cheap content-quality measures captured at parse time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct QualitySignals {
    pub text_ratio: f64,
    pub link_density: f64,
    pub scripts: usize,
    pub words: usize,
}

/// One crawled page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    /// The URL we asked for.
    pub url: String,
    /// Where we ended up, if redirects moved us.
    pub final_url: String,
    /// Unix seconds.
    pub fetched_at: u64,
    pub status: u16,
    /// Hops from a seed.
    pub depth: u32,
    /// `SimHash` fingerprint of the text, for near-duplicate detection.
    pub fingerprint: u64,
    pub title: String,
    pub text: String,
    pub headings: Vec<String>,
    pub lang: Option<String>,
    pub links: Vec<OutLink>,
    pub quality: QualitySignals,
    /// False when the page sent `nosnippet`. Carried through to the front end
    /// so the directive survives into what we actually display.
    pub snippet_allowed: bool,
    /// Longest snippet the page permits, in characters.
    pub max_snippet: Option<usize>,
}

/// Summary of a crawl, written alongside the records for humans to read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlSummary {
    pub started_at: u64,
    pub finished_at: u64,
    pub pages_stored: usize,
    pub pages_fetched: usize,
    pub fetch_failures: usize,
    pub robots_disallowed: usize,
    pub near_duplicates: usize,
    pub noindex: usize,
    pub hosts: usize,
    pub urls_seen: usize,
    /// Why URLs were turned away, by category.
    pub refusals: std::collections::BTreeMap<String, usize>,
    /// Why fetches failed, by category.
    pub failures: std::collections::BTreeMap<String, usize>,
    pub bytes_written: u64,
    pub text_bytes: u64,
}

/// Appends records to a store.
#[derive(Debug)]
pub struct StoreWriter {
    pages: BufWriter<File>,
    index: BufWriter<File>,
    dir: PathBuf,
    /// Records buffered for the block currently being built.
    pending: Vec<u8>,
    pending_count: u32,
    /// Byte offset of the block currently being built.
    block_offset: u64,
    next_doc_id: u32,
    text_bytes: u64,
}

impl StoreWriter {
    /// Create a store in `dir`, replacing anything already there.
    pub fn create(dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir)?;
        let mut pages = BufWriter::new(File::create(dir.join("pages.uruk"))?);
        let mut index = BufWriter::new(File::create(dir.join("pages.idx"))?);

        pages.write_all(PAGES_MAGIC)?;
        pages.write_all(&FORMAT_VERSION.to_le_bytes())?;
        index.write_all(INDEX_MAGIC)?;
        index.write_all(&FORMAT_VERSION.to_le_bytes())?;

        let header_len = (PAGES_MAGIC.len() + size_of::<u32>()) as u64;
        Ok(Self {
            pages,
            index,
            dir: dir.to_path_buf(),
            pending: Vec::with_capacity(BLOCK_TARGET + 8192),
            pending_count: 0,
            block_offset: header_len,
            next_doc_id: 0,
            text_bytes: 0,
        })
    }

    /// Append one record. Returns its document id.
    pub fn push(&mut self, record: &Record) -> Result<u32, StoreError> {
        let line = serde_json::to_vec(record)?;
        self.text_bytes += record.text.len() as u64;

        // Index entries point at the block and the record's position inside
        // it, so both must be known before the record is appended.
        self.index.write_all(&self.block_offset.to_le_bytes())?;
        self.index.write_all(&self.pending_count.to_le_bytes())?;

        self.pending.extend_from_slice(&line);
        self.pending.push(b'\n');
        self.pending_count += 1;

        let doc_id = self.next_doc_id;
        self.next_doc_id += 1;

        if self.pending.len() >= BLOCK_TARGET {
            self.flush_block()?;
        }
        Ok(doc_id)
    }

    fn flush_block(&mut self) -> Result<(), StoreError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let compressed = zstd::bulk::compress(&self.pending, ZSTD_LEVEL)?;
        let uncompressed_len = u32::try_from(self.pending.len())
            .map_err(|_| StoreError::Corrupt("block larger than 4 GiB".into()))?;
        let compressed_len = u32::try_from(compressed.len())
            .map_err(|_| StoreError::Corrupt("compressed block larger than 4 GiB".into()))?;

        self.pages.write_all(&uncompressed_len.to_le_bytes())?;
        self.pages.write_all(&compressed_len.to_le_bytes())?;
        self.pages.write_all(&compressed)?;

        self.block_offset += (2 * size_of::<u32>() + compressed.len()) as u64;
        self.pending.clear();
        self.pending_count = 0;
        Ok(())
    }

    /// Flush the final block and write the crawl summary.
    ///
    /// Returns the summary as written, with the fields only the store can
    /// know (`pages_stored`, `bytes_written`, `text_bytes`) filled in. Callers
    /// should use the returned value rather than the one they passed in.
    ///
    /// Must be called; dropping the writer leaves the last block unwritten.
    pub fn finish(mut self, summary: &CrawlSummary) -> Result<CrawlSummary, StoreError> {
        self.flush_block()?;
        self.pages.flush()?;
        self.index.flush()?;

        let mut summary = summary.clone();
        summary.pages_stored = self.next_doc_id as usize;
        summary.bytes_written = self.block_offset;
        summary.text_bytes = self.text_bytes;

        let json = serde_json::to_string_pretty(&summary)?;
        std::fs::write(self.dir.join("crawl.json"), json)?;
        Ok(summary)
    }

    /// Documents written so far.
    pub fn len(&self) -> u32 {
        self.next_doc_id
    }

    pub fn is_empty(&self) -> bool {
        self.next_doc_id == 0
    }
}

/// Reads records back, either in order or one at a time.
#[derive(Debug)]
pub struct StoreReader {
    pages: BufReader<File>,
    /// `(block_offset, ordinal within block)` per document.
    index: Vec<(u64, u32)>,
}

impl StoreReader {
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        let pages_path = dir.join("pages.uruk");
        let index_path = dir.join("pages.idx");

        let mut pages = BufReader::new(File::open(&pages_path)?);
        check_header(&mut pages, *PAGES_MAGIC, &pages_path)?;

        let mut index_file = BufReader::new(File::open(&index_path)?);
        check_header(&mut index_file, *INDEX_MAGIC, &index_path)?;

        let mut raw = Vec::new();
        index_file.read_to_end(&mut raw)?;
        if raw.len() % INDEX_ENTRY != 0 {
            return Err(StoreError::Corrupt(format!(
                "index length {} is not a multiple of {INDEX_ENTRY}",
                raw.len()
            )));
        }

        let index = raw
            .chunks_exact(INDEX_ENTRY)
            .map(|entry| {
                let offset = u64::from_le_bytes(entry[0..8].try_into().expect("8 bytes"));
                let ordinal = u32::from_le_bytes(entry[8..12].try_into().expect("4 bytes"));
                (offset, ordinal)
            })
            .collect();

        Ok(Self { pages, index })
    }

    /// Number of documents in the store.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Read one document, decompressing only the block it lives in.
    pub fn get(&mut self, doc_id: u32) -> Result<Record, StoreError> {
        let &(offset, ordinal) = self
            .index
            .get(doc_id as usize)
            .ok_or(StoreError::NoSuchDocument(doc_id))?;

        let block = self.read_block(offset)?;
        let line = block
            .split(|&byte| byte == b'\n')
            .nth(ordinal as usize)
            .ok_or_else(|| {
                StoreError::Corrupt(format!("document {doc_id} missing from its block"))
            })?;
        Ok(serde_json::from_slice(line)?)
    }

    /// Every record in order. Reads one block at a time rather than holding
    /// the corpus in memory.
    pub fn records(&mut self) -> Result<RecordIter<'_>, StoreError> {
        let header_len = (PAGES_MAGIC.len() + size_of::<u32>()) as u64;
        self.pages.seek(SeekFrom::Start(header_len))?;
        Ok(RecordIter {
            reader: self,
            buffer: Vec::new(),
            position: 0,
            done: false,
        })
    }

    fn read_block(&mut self, offset: u64) -> Result<Vec<u8>, StoreError> {
        self.pages.seek(SeekFrom::Start(offset))?;
        read_block_at(&mut self.pages)?
            .ok_or_else(|| StoreError::Corrupt(format!("no block at offset {offset}")))
    }
}

/// Read one block from the current position. `None` at clean end of file.
fn read_block_at(reader: &mut BufReader<File>) -> Result<Option<Vec<u8>>, StoreError> {
    let mut lengths = [0u8; 8];
    match reader.read_exact(&mut lengths) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let uncompressed_len = u32::from_le_bytes(lengths[0..4].try_into().expect("4 bytes")) as usize;
    let compressed_len = u32::from_le_bytes(lengths[4..8].try_into().expect("4 bytes")) as usize;

    let mut compressed = vec![0u8; compressed_len];
    reader.read_exact(&mut compressed)?;
    Ok(Some(zstd::bulk::decompress(&compressed, uncompressed_len)?))
}

/// Sequential reader over every record.
#[derive(Debug)]
pub struct RecordIter<'a> {
    reader: &'a mut StoreReader,
    buffer: Vec<u8>,
    position: usize,
    done: bool,
}

impl Iterator for RecordIter<'_> {
    type Item = Result<Record, StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.position < self.buffer.len() {
                let rest = &self.buffer[self.position..];
                let end = rest
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .unwrap_or(rest.len());
                let line = &rest[..end];
                self.position += end + 1;
                if line.is_empty() {
                    continue;
                }
                return Some(serde_json::from_slice(line).map_err(StoreError::from));
            }
            if self.done {
                return None;
            }
            match read_block_at(&mut self.reader.pages) {
                Ok(Some(block)) => {
                    self.buffer = block;
                    self.position = 0;
                }
                Ok(None) => {
                    self.done = true;
                    return None;
                }
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

fn check_header(
    reader: &mut BufReader<File>,
    magic: [u8; 8],
    path: &Path,
) -> Result<(), StoreError> {
    let mut found = [0u8; 8];
    reader
        .read_exact(&mut found)
        .map_err(|_| StoreError::BadMagic {
            path: path.to_path_buf(),
        })?;
    if found != magic {
        return Err(StoreError::BadMagic {
            path: path.to_path_buf(),
        });
    }
    let mut version = [0u8; 4];
    reader.read_exact(&mut version)?;
    let version = u32::from_le_bytes(version);
    if version != FORMAT_VERSION {
        return Err(StoreError::VersionMismatch {
            path: path.to_path_buf(),
            found: version,
            expected: FORMAT_VERSION,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CrawlSummary, OutLink, QualitySignals, Record, StoreError, StoreReader, StoreWriter,
    };

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("uruk-store-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn record(n: usize) -> Record {
        Record {
            url: format!("https://a.test/page/{n}"),
            final_url: format!("https://a.test/page/{n}"),
            fetched_at: 1_700_000_000 + n as u64,
            status: 200,
            depth: 1,
            fingerprint: 0x1234_5678_9abc_def0 ^ n as u64,
            title: format!("Page {n}"),
            text: format!(
                "Document number {n}. The scribes of Uruk pressed reed into clay and \
                 recorded quantities of barley, which is how the receipt was invented. \
                 This sentence exists so the record has enough text to be worth compressing."
            ),
            headings: vec![format!("Heading {n}")],
            lang: Some("en".into()),
            links: vec![OutLink {
                url: format!("https://a.test/page/{}", n + 1),
                anchor: "next".into(),
                nofollow: false,
            }],
            quality: QualitySignals {
                text_ratio: 0.42,
                link_density: 0.1,
                scripts: 2,
                words: 30,
            },
            snippet_allowed: true,
            max_snippet: None,
        }
    }

    fn write_store(dir: &std::path::Path, count: usize) {
        let mut writer = StoreWriter::create(dir).unwrap();
        for n in 0..count {
            assert_eq!(writer.push(&record(n)).unwrap(), u32::try_from(n).unwrap());
        }
        assert_eq!(writer.len(), u32::try_from(count).unwrap());
        writer.finish(&CrawlSummary::default()).unwrap();
    }

    #[test]
    fn round_trips_a_single_record() {
        let dir = temp_dir("single");
        write_store(&dir, 1);

        let mut reader = StoreReader::open(&dir).unwrap();
        assert_eq!(reader.len(), 1);
        assert_eq!(reader.get(0).unwrap(), record(0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trips_enough_records_to_span_many_blocks() {
        // 4,000 records of a few hundred bytes each comfortably exceeds the
        // 256 KiB block target, so this exercises block boundaries.
        let dir = temp_dir("many");
        write_store(&dir, 4_000);

        let mut reader = StoreReader::open(&dir).unwrap();
        assert_eq!(reader.len(), 4_000);

        let all: Vec<_> = reader.records().unwrap().map(Result::unwrap).collect();
        assert_eq!(all.len(), 4_000);
        assert_eq!(all[0], record(0));
        assert_eq!(all[3_999], record(3_999));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn random_access_works_across_block_boundaries() {
        let dir = temp_dir("random");
        write_store(&dir, 4_000);

        let mut reader = StoreReader::open(&dir).unwrap();
        // Deliberately out of order, and revisiting a block already read.
        for n in [3_999usize, 0, 2_500, 1, 2_500, 137] {
            assert_eq!(
                reader.get(u32::try_from(n).unwrap()).unwrap(),
                record(n),
                "doc {n}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_store_is_valid() {
        let dir = temp_dir("empty");
        write_store(&dir, 0);

        let mut reader = StoreReader::open(&dir).unwrap();
        assert!(reader.is_empty());
        assert_eq!(reader.records().unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reading_past_the_end_is_an_error_not_a_panic() {
        let dir = temp_dir("oob");
        write_store(&dir, 3);

        let mut reader = StoreReader::open(&dir).unwrap();
        assert!(matches!(
            reader.get(99),
            Err(StoreError::NoSuchDocument(99))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_foreign_file_is_rejected_rather_than_misread() {
        let dir = temp_dir("magic");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pages.uruk"), b"definitely not a uruk store").unwrap();
        std::fs::write(dir.join("pages.idx"), b"nor is this one").unwrap();

        assert!(matches!(
            StoreReader::open(&dir),
            Err(StoreError::BadMagic { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finish_returns_the_summary_it_wrote() {
        // Regression: run() used to return a snapshot taken before finish(),
        // so bytes_written and text_bytes were always zero to callers.
        let dir = temp_dir("finish-returns");
        let mut writer = StoreWriter::create(&dir).unwrap();
        for n in 0..10 {
            writer.push(&record(n)).unwrap();
        }
        let returned = writer.finish(&CrawlSummary::default()).unwrap();
        assert_eq!(returned.pages_stored, 10);
        assert!(returned.bytes_written > 0);
        assert!(returned.text_bytes > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_summary_records_what_was_written() {
        let dir = temp_dir("summary");
        write_store(&dir, 100);

        let raw = std::fs::read_to_string(dir.join("crawl.json")).unwrap();
        let summary: CrawlSummary = serde_json::from_str(&raw).unwrap();
        assert_eq!(summary.pages_stored, 100);
        assert!(summary.bytes_written > 0);
        assert!(summary.text_bytes > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compression_actually_compresses() {
        // The whole point of the store. Article text is prose and should come
        // down by several times; if this regresses, something is writing the
        // blocks uncompressed.
        let dir = temp_dir("ratio");
        write_store(&dir, 2_000);

        let raw = std::fs::read_to_string(dir.join("crawl.json")).unwrap();
        let summary: CrawlSummary = serde_json::from_str(&raw).unwrap();
        let on_disk = std::fs::metadata(dir.join("pages.uruk")).unwrap().len();

        assert!(
            on_disk < summary.text_bytes / 2,
            "store is {on_disk} bytes for {} bytes of text",
            summary.text_bytes
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
