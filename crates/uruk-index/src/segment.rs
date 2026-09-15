//! A `.uruk` segment: one immutable, self-contained piece of the index.
//!
//! Segments are the structural idea worth stealing from Lucene
//! (`RESEARCH.md` §2.3). A segment is written once and never modified. New
//! crawl data makes a new segment; nothing rewrites what is already there.
//! Readers hold a fixed set of files, so writing never blocks reading, a
//! half-written segment is simply one nothing references yet, and compression
//! works on whole finished batches rather than bytes patched in place.
//!
//! # Layout
//!
//! One file, four sections, and a fixed-size footer at the end that says where
//! each section starts. The footer is last because the offsets are not known
//! until the data has been written.
//!
//! ```text
//! [ magic "URUKIDX1" ][ version u32 ]
//! [ postings   ] one encoded list per term, in dictionary order
//! [ dictionary ] front-coded sorted terms -> postings offset, doc frequency
//! [ hosts      ] front-coded sorted host names, for `site:` filtering
//! [ doc table  ] per document: crawl store id, host, lengths, quality
//! [ footer     ] section offsets, counts, corpus totals, magic again
//! ```
//!
//! Hosts are a separate table rather than a string per document because a
//! segment of fifty thousand pages comes from far fewer than fifty thousand
//! sites. A document stores a small integer, and `site:` becomes an integer
//! comparison rather than a string parse per candidate.
//!
//! **Front coding** on the dictionary means each term stores only what it does
//! not share with the previous one: after `tablet`, the term `tablets` costs a
//! shared-prefix length of 6 and the single byte `s`. Sorted term lists share
//! long prefixes, so this is most of the dictionary's size gone for almost
//! nothing.
//!
//! # What is loaded, and what is not
//!
//! Opening a segment reads the dictionary and the document table into memory
//! and leaves the postings on disk. A query then reads only the lists for the
//! terms it actually mentions, which is the rule from `RESEARCH.md` §5.5: the
//! index is never wholly resident, and nothing is decoded that a query did not
//! ask for.
//!
//! The honest limitation is the dictionary. Holding it in memory is fine for
//! the millions of terms a first corpus produces and will stop being fine
//! somewhere past that; the fix is a block-based on-disk dictionary with
//! binary search, which is Phase 5 work and does not change this interface.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fields::{FIELD_COUNT, Field, FieldCounts, FieldLengths};
use crate::postings::{self, DocPosting, Posting, PostingList, read_varint, write_varint};
use crate::tokenize::{self, Token};

/// Positions left empty between one field and the next.
///
/// Wide enough that no phrase can straddle a field boundary and that a term in
/// the title and one in the body never look adjacent to proximity scoring.
const FIELD_POSITION_GAP: u32 = 1_000;

const MAGIC: &[u8; 8] = b"URUKIDX1";

/// On-disk format version.
///
/// **Bump this on every encoding change, without exception.** A segment whose
/// bytes mean something different but whose version still matches is not
/// rejected — it is decoded, silently, into wrong answers. That happened once
/// during development: the host table and the position scheme both changed
/// while this stayed at 1, and a stale index went on being read as if nothing
/// had, returning fewer results than it should with no error anywhere.
///
/// History:
/// - 1: first format.
/// - 2: host table added; positions kept for every field rather than the body
///   alone; the redundant per-posting position count removed.
/// - 3: postings split into separate document-id, frequency and position
///   streams so block codecs have runs long enough to pay off, with the field
///   mask written only for postings that touch a field other than the body.
/// - 4: positions cut into independently decodable groups with a byte-length
///   table, so a reader can fetch one document's positions without decoding
///   every position before it.
const FORMAT_VERSION: u32 = 4;
/// 4 section offsets + term count + 4 corpus totals + doc count + magic.
const FOOTER_LEN: u64 = 8 * 5 + 8 * FIELD_COUNT as u64 + 4 + 8;

#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    #[error("segment I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not read the segment manifest: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{path} is not a uruk segment")]
    BadMagic { path: PathBuf },
    #[error("{path} is segment format version {found}, but this build understands {expected}")]
    VersionMismatch {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
    #[error("segment is corrupt: {0}")]
    Corrupt(String),
    #[error("posting list for {term:?} is corrupt: {source}")]
    BadPostings {
        term: String,
        source: postings::DecodeError,
    },
}

/// A document being added to a segment.
#[derive(Debug, Clone, Copy)]
pub struct Document<'a> {
    /// Which record this is in the crawl store, so results can find their text.
    pub crawl_doc: u32,
    pub url: &'a str,
    pub title: &'a str,
    pub headings: &'a [String],
    pub body: &'a str,
    /// The host this page came from, for `site:` filtering.
    pub host: &'a str,
    pub quality: DocQuality,
}

/// Cheap content-quality proxies, carried from the crawl into the index.
///
/// The brief lists these as the lowest-weighted ranking signal: pages that are
/// mostly chrome should rank below pages that are mostly writing. They live in
/// the doc table rather than being re-read from the crawl store at query time,
/// because scoring touches every candidate and random reads into a compressed
/// store would dominate the latency budget.
///
/// Stored as thousandths so the doc table stays varints rather than floats.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DocQuality {
    /// Extracted text length over total HTML length.
    pub text_ratio: f32,
    /// Fraction of the article's words that sit inside a link.
    pub link_density: f32,
    /// Number of `<script>` elements.
    pub scripts: u32,
}

/// Thousandths, clamped: the doc table stores these as small integers.
fn to_thousandths(value: f32) -> u32 {
    if value.is_finite() && value > 0.0 {
        #[expect(clippy::cast_possible_truncation, reason = "clamped to 0..=1000")]
        #[expect(clippy::cast_sign_loss, reason = "positive by the guard above")]
        let scaled = (value * 1000.0).min(1000.0) as u32;
        scaled
    } else {
        0
    }
}

fn from_thousandths(value: u32) -> f32 {
    value as f32 / 1000.0
}

/// Where a document lives and how long it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DocEntry {
    pub crawl_doc: u32,
    /// Index into the segment's host table.
    pub host: u32,
    /// Tokens per field. BM25 normalises by this: two occurrences in a
    /// ten-word title mean more than two in a thousand-word body.
    pub lengths: FieldLengths,
    pub quality: DocQuality,
}

/// Readable summary written next to the segment.
///
/// JSON on purpose: someone debugging an index should be able to see what is
/// in it without a tool, and the size breakdown is the number Phase 5 will be
/// trying to move.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentManifest {
    pub documents: u32,
    pub terms: u64,
    pub postings: u64,
    pub bytes_total: u64,
    pub bytes_postings: u64,
    pub bytes_dictionary: u64,
    #[serde(default)]
    pub bytes_hosts: u64,
    pub bytes_doc_table: u64,
    /// Total tokens per field, for BM25's average document length.
    pub tokens_per_field: BTreeMap<String, u64>,
}

/// Accumulates documents, then writes a segment.
///
/// Everything is held in memory until [`Self::write`], which is the right
/// shape for segments sized to fit and the reason segments exist: a crawl
/// produces several rather than one enormous one.
#[derive(Debug, Default)]
pub struct SegmentBuilder {
    /// `BTreeMap` rather than `HashMap` so the dictionary comes out sorted,
    /// which front coding and binary search both require.
    terms: BTreeMap<String, Vec<Posting>>,
    docs: Vec<DocEntry>,
    tokens_per_field: [u64; FIELD_COUNT],
    /// Host name -> the id stored in the doc table. Resolved to sorted order
    /// when the segment is written.
    hosts: BTreeMap<String, u32>,
}

impl SegmentBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Add a document. Returns its id within this segment.
    ///
    /// Ids are assigned in insertion order, which is what makes posting lists
    /// naturally sorted and delta encoding possible.
    pub fn add(&mut self, document: &Document<'_>) -> u32 {
        let id = u32::try_from(self.docs.len()).expect("a segment holds fewer than 4 billion docs");

        let headings = document.headings.join(" ");
        // Body first, deliberately: positions are delta-encoded from zero, so
        // whichever field leads gets the smallest numbers, and the body holds
        // almost all the occurrences. (Measured at 20,000 documents the
        // ordering made no difference to the total — body positions already
        // run past 128 and cost two bytes either way — but it is the right
        // default and costs nothing.)
        let per_field: [(Field, Vec<Token>); FIELD_COUNT] = [
            (Field::Body, tokenize::tokenize(document.body)),
            (Field::Title, tokenize::tokenize(document.title)),
            (Field::Heading, tokenize::tokenize(&headings)),
            (Field::Url, tokenize::tokenize_url(document.url)),
        ];

        // Fields share one position space so that a phrase can match inside
        // any of them, with a gap between fields wide enough that no phrase
        // and no useful proximity can straddle two.
        let mut base = 0u32;

        let mut lengths = FieldLengths::default();
        for (field, tokens) in &per_field {
            // Length counts positions, not surviving tokens, so that junk
            // dropped by the tokeniser still contributes to document length.
            let length = tokens.last().map_or(0, |token| token.position + 1);
            lengths.set(*field, length);
            self.tokens_per_field[field.index()] += u64::from(length);

            for token in tokens {
                let entries = self.terms.entry(token.term.clone()).or_default();
                // Postings arrive in document order, so the entry for this
                // document, if any, can only be the final one.
                let posting = match entries.last_mut() {
                    Some(tail) if tail.doc == id => tail,
                    _ => {
                        entries.push(Posting::new(id));
                        entries.last_mut().expect("just pushed")
                    }
                };
                posting.counts.add(*field, 1);
                posting.positions.push(base + token.position);
            }
            base = base
                .saturating_add(length)
                .saturating_add(FIELD_POSITION_GAP);
        }

        // Hosts are interned here and renumbered into sorted order at write
        // time, so the on-disk table can be front-coded and binary-searched.
        let next_host = u32::try_from(self.hosts.len()).unwrap_or(u32::MAX);
        let host = *self
            .hosts
            .entry(document.host.to_ascii_lowercase())
            .or_insert(next_host);

        self.docs.push(DocEntry {
            crawl_doc: document.crawl_doc,
            host,
            lengths,
            quality: document.quality,
        });
        id
    }

    /// Write the segment to `path`, plus a `.json` manifest beside it.
    ///
    /// Sections are written in order and each one reports its size, because
    /// the footer's offsets are running totals that cannot be known until the
    /// data ahead of them exists.
    pub fn write(&self, path: &Path) -> Result<SegmentManifest, SegmentError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = BufWriter::new(File::create(path)?);
        let mut buffer = Vec::new();

        file.write_all(MAGIC)?;
        file.write_all(&FORMAT_VERSION.to_le_bytes())?;
        let header_len = MAGIC.len() as u64 + 4;

        let (offsets, posting_count) = self.encode_postings(&mut buffer);
        let bytes_postings = flush_section(&mut file, &buffer)?;

        let dict_offset = header_len + bytes_postings;
        self.encode_dictionary(&offsets, &mut buffer);
        let bytes_dictionary = flush_section(&mut file, &buffer)?;

        let hosts_offset = dict_offset + bytes_dictionary;
        let renumbered = self.encode_hosts(&mut buffer);
        let bytes_hosts = flush_section(&mut file, &buffer)?;

        let docs_offset = hosts_offset + bytes_hosts;
        self.encode_doc_table(&renumbered, &mut buffer);
        let bytes_doc_table = flush_section(&mut file, &buffer)?;

        // --- footer ---
        for offset in [header_len, dict_offset, hosts_offset, docs_offset] {
            file.write_all(&offset.to_le_bytes())?;
        }
        file.write_all(&(self.terms.len() as u64).to_le_bytes())?;
        for total in self.tokens_per_field {
            file.write_all(&total.to_le_bytes())?;
        }
        file.write_all(
            &u32::try_from(self.docs.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        )?;
        file.write_all(MAGIC)?;
        file.flush()?;

        let manifest = SegmentManifest {
            documents: u32::try_from(self.docs.len()).unwrap_or(u32::MAX),
            terms: self.terms.len() as u64,
            postings: posting_count,
            bytes_total: header_len
                + bytes_postings
                + bytes_dictionary
                + bytes_hosts
                + bytes_doc_table
                + FOOTER_LEN,
            bytes_postings,
            bytes_dictionary,
            bytes_hosts,
            bytes_doc_table,
            tokens_per_field: Field::ALL
                .iter()
                .map(|field| {
                    (
                        field.name().to_owned(),
                        self.tokens_per_field[field.index()],
                    )
                })
                .collect(),
        };
        std::fs::write(
            path.with_extension("json"),
            serde_json::to_string_pretty(&manifest)?,
        )?;
        Ok(manifest)
    }

    /// Encode every posting list, returning where each one starts and how many
    /// postings were written in total.
    fn encode_postings(&self, buffer: &mut Vec<u8>) -> (Vec<u64>, u64) {
        buffer.clear();
        let mut offsets = Vec::with_capacity(self.terms.len());
        let mut posting_count = 0u64;
        for list in self.terms.values() {
            offsets.push(buffer.len() as u64);
            posting_count += list.len() as u64;
            postings::encode(list, buffer);
        }
        (offsets, posting_count)
    }

    /// Front-coded terms, each with the offset of its posting list.
    fn encode_dictionary(&self, offsets: &[u64], buffer: &mut Vec<u8>) {
        buffer.clear();
        write_varint(buffer, self.terms.len() as u64);
        let mut previous_term = String::new();
        let mut previous_offset = 0u64;
        for ((term, list), &offset) in self.terms.iter().zip(offsets) {
            write_front_coded(buffer, previous_term.as_bytes(), term.as_bytes());
            // Offsets ascend, so store the gap rather than the number.
            write_varint(buffer, offset - previous_offset);
            write_varint(buffer, list.len() as u64);
            previous_term.clone_from(term);
            previous_offset = offset;
        }
    }

    /// Front-coded host names, and the map from interned ids to sorted ones.
    ///
    /// Interning handed out ids in first-seen order. Sorting them — which
    /// front coding and binary search both need — renumbers them, so the doc
    /// table has to be written with the new numbers.
    fn encode_hosts(&self, buffer: &mut Vec<u8>) -> HashMap<u32, u32> {
        buffer.clear();
        write_varint(buffer, self.hosts.len() as u64);
        let mut previous = String::new();
        let mut renumbered = HashMap::with_capacity(self.hosts.len());
        for (sorted, (host, interned)) in self.hosts.iter().enumerate() {
            write_front_coded(buffer, previous.as_bytes(), host.as_bytes());
            renumbered.insert(*interned, u32::try_from(sorted).unwrap_or(u32::MAX));
            previous.clone_from(host);
        }
        renumbered
    }

    fn encode_doc_table(&self, renumbered: &HashMap<u32, u32>, buffer: &mut Vec<u8>) {
        buffer.clear();
        write_varint(buffer, self.docs.len() as u64);
        for entry in &self.docs {
            write_varint(buffer, u64::from(entry.crawl_doc));
            write_varint(
                buffer,
                u64::from(renumbered.get(&entry.host).copied().unwrap_or(0)),
            );
            for field in 0..FIELD_COUNT {
                write_varint(buffer, u64::from(entry.lengths.get_index(field)));
            }
            write_varint(buffer, u64::from(to_thousandths(entry.quality.text_ratio)));
            write_varint(
                buffer,
                u64::from(to_thousandths(entry.quality.link_density)),
            );
            write_varint(buffer, u64::from(entry.quality.scripts));
        }
    }
}

/// Write a byte slice as one section, returning its length.
fn flush_section(file: &mut BufWriter<File>, buffer: &[u8]) -> Result<u64, SegmentError> {
    file.write_all(buffer)?;
    Ok(buffer.len() as u64)
}

/// Append `current`, storing only what it does not share with `previous`.
fn write_front_coded(buffer: &mut Vec<u8>, previous: &[u8], current: &[u8]) {
    let shared = shared_prefix(previous, current);
    write_varint(buffer, shared as u64);
    let suffix = &current[shared..];
    write_varint(buffer, suffix.len() as u64);
    buffer.extend_from_slice(suffix);
}

fn shared_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// One term's entry in the in-memory dictionary.
#[derive(Debug, Clone)]
struct TermEntry {
    term: String,
    /// Byte offset of its posting list, relative to the postings section.
    offset: u64,
    /// Length of the encoded list, derived from the next term's offset.
    length: u64,
    doc_freq: u32,
}

/// Reads a written segment.
#[derive(Debug)]
pub struct SegmentReader {
    file: File,
    postings_offset: u64,
    dictionary: Vec<TermEntry>,
    /// Sorted, so a `site:` filter is one binary search per query rather than
    /// a string comparison per candidate document.
    hosts: Vec<String>,
    docs: Vec<DocEntry>,
    tokens_per_field: [u64; FIELD_COUNT],
}

impl SegmentReader {
    pub fn open(path: &Path) -> Result<Self, SegmentError> {
        let mut file = File::open(path)?;

        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)
            .map_err(|_| SegmentError::BadMagic { path: path.into() })?;
        if &magic != MAGIC {
            return Err(SegmentError::BadMagic { path: path.into() });
        }
        let mut version = [0u8; 4];
        file.read_exact(&mut version)?;
        let version = u32::from_le_bytes(version);
        if version != FORMAT_VERSION {
            return Err(SegmentError::VersionMismatch {
                path: path.into(),
                found: version,
                expected: FORMAT_VERSION,
            });
        }

        let file_len = file.metadata()?.len();
        if file_len < FOOTER_LEN {
            return Err(SegmentError::Corrupt(
                "file is shorter than its footer".into(),
            ));
        }
        file.seek(SeekFrom::Start(file_len - FOOTER_LEN))?;
        let mut footer = vec![0u8; usize::try_from(FOOTER_LEN).expect("footer fits in memory")];
        file.read_exact(&mut footer)?;

        if &footer[footer.len() - 8..] != MAGIC {
            return Err(SegmentError::Corrupt(
                "footer magic missing; file is truncated".into(),
            ));
        }
        let read_u64 = |at: usize| u64::from_le_bytes(footer[at..at + 8].try_into().expect("8"));

        let postings_offset = read_u64(0);
        let dict_offset = read_u64(8);
        let hosts_offset = read_u64(16);
        let docs_offset = read_u64(24);
        let term_count = read_u64(32);
        let mut tokens_per_field = [0u64; FIELD_COUNT];
        for (index, total) in tokens_per_field.iter_mut().enumerate() {
            *total = read_u64(40 + index * 8);
        }

        let footer_start = file_len - FOOTER_LEN;
        if !(postings_offset <= dict_offset
            && dict_offset <= hosts_offset
            && hosts_offset <= docs_offset
            && docs_offset <= footer_start)
        {
            return Err(SegmentError::Corrupt(
                "section offsets are out of order".into(),
            ));
        }

        let dictionary = read_dictionary(
            &read_section(&mut file, dict_offset, hosts_offset)?,
            term_count,
            dict_offset - postings_offset,
        )?;
        let hosts = read_hosts(&read_section(&mut file, hosts_offset, docs_offset)?)?;
        let docs = read_doc_table(&read_section(&mut file, docs_offset, footer_start)?)?;

        Ok(Self {
            file,
            postings_offset,
            dictionary,
            hosts,
            docs,
            tokens_per_field,
        })
    }

    /// Documents in this segment.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Distinct terms.
    pub fn terms(&self) -> usize {
        self.dictionary.len()
    }

    pub fn doc(&self, doc: u32) -> Option<DocEntry> {
        self.docs.get(doc as usize).copied()
    }

    /// The id a `site:` filter should match, or `None` if this segment holds
    /// nothing from that host — in which case the whole segment can be skipped.
    pub fn host_id(&self, host: &str) -> Option<u32> {
        let host = host.to_ascii_lowercase();
        self.hosts
            .binary_search_by(|candidate| candidate.as_str().cmp(host.as_str()))
            .ok()
            .and_then(|index| u32::try_from(index).ok())
    }

    /// A document's host name.
    pub fn host_name(&self, id: u32) -> Option<&str> {
        self.hosts.get(id as usize).map(String::as_str)
    }

    /// Distinct hosts in this segment.
    pub fn host_count(&self) -> usize {
        self.hosts.len()
    }

    /// Average tokens per document in a field, which BM25 normalises against.
    pub fn average_length(&self, field: Field) -> f64 {
        if self.docs.is_empty() {
            return 0.0;
        }
        self.tokens_per_field[field.index()] as f64 / self.docs.len() as f64
    }

    /// How many documents contain `term`. The input to IDF, and cheap: it
    /// comes from the dictionary without touching the postings.
    pub fn doc_frequency(&self, term: &str) -> u32 {
        self.find(term)
            .map_or(0, |entry| self.dictionary[entry].doc_freq)
    }

    fn find(&self, term: &str) -> Option<usize> {
        self.dictionary
            .binary_search_by(|entry| entry.term.as_str().cmp(term))
            .ok()
    }

    /// Read one term's posting list, and nothing else.
    ///
    /// This is the whole point of the layout: a query for three words reads
    /// three lists, not the index.
    pub fn postings(&mut self, term: &str) -> Result<Vec<Posting>, SegmentError> {
        let Some(bytes) = self.posting_bytes(term)? else {
            return Ok(Vec::new());
        };
        let mut cursor = 0;
        postings::decode(&bytes, &mut cursor).map_err(|source| SegmentError::BadPostings {
            term: term.to_owned(),
            source,
        })
    }

    /// One term's postings without the position stream.
    ///
    /// Reads exactly the same bytes off disk — a posting list is one contiguous
    /// run and seeking past part of it would cost more than reading it — but
    /// skips decoding the positions, which is where the time goes.
    pub fn document_postings(&mut self, term: &str) -> Result<Vec<DocPosting>, SegmentError> {
        let Some(bytes) = self.posting_bytes(term)? else {
            return Ok(Vec::new());
        };
        let mut cursor = 0;
        postings::decode_documents(&bytes, &mut cursor).map_err(|source| {
            SegmentError::BadPostings {
                term: term.to_owned(),
                source,
            }
        })
    }

    /// One term's postings with the positions left unread until asked for.
    ///
    /// The shape the query path wants: it needs every candidate's frequencies
    /// to rank them, and only the survivors' positions.
    pub fn posting_list(&mut self, term: &str) -> Result<PostingList, SegmentError> {
        let Some(bytes) = self.posting_bytes(term)? else {
            return Ok(PostingList::default());
        };
        let mut cursor = 0;
        postings::decode_list(&bytes, &mut cursor).map_err(|source| SegmentError::BadPostings {
            term: term.to_owned(),
            source,
        })
    }

    /// The raw bytes of one term's posting list, or `None` if it has none.
    fn posting_bytes(&mut self, term: &str) -> Result<Option<Vec<u8>>, SegmentError> {
        let Some(index) = self.find(term) else {
            return Ok(None);
        };
        let entry = self.dictionary[index].clone();

        self.file
            .seek(SeekFrom::Start(self.postings_offset + entry.offset))?;
        let mut bytes = vec![0u8; usize::try_from(entry.length).unwrap_or(0)];
        self.file.read_exact(&mut bytes)?;
        Ok(Some(bytes))
    }

    /// Every term, in sorted order. For debugging and for the CLI.
    pub fn term_list(&self) -> impl Iterator<Item = (&str, u32)> {
        self.dictionary
            .iter()
            .map(|entry| (entry.term.as_str(), entry.doc_freq))
    }
}

fn read_section(file: &mut File, from: u64, to: u64) -> Result<Vec<u8>, SegmentError> {
    let length = to
        .checked_sub(from)
        .ok_or_else(|| SegmentError::Corrupt("negative section length".into()))?;
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = vec![0u8; usize::try_from(length).unwrap_or(0)];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_dictionary(
    bytes: &[u8],
    expected_terms: u64,
    postings_len: u64,
) -> Result<Vec<TermEntry>, SegmentError> {
    let corrupt = |what: &str| SegmentError::Corrupt(format!("dictionary: {what}"));

    let mut cursor = 0usize;
    let count = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated term count"))?;
    if count != expected_terms {
        return Err(corrupt("term count disagrees with the footer"));
    }
    let count = usize::try_from(count).map_err(|_| corrupt("implausible term count"))?;
    if count > bytes.len() + 1 {
        return Err(corrupt("term count exceeds the section size"));
    }

    let mut entries: Vec<TermEntry> = Vec::with_capacity(count);
    let mut previous = Vec::<u8>::new();
    let mut offset = 0u64;

    for _ in 0..count {
        let shared = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated prefix"))?;
        let shared = usize::try_from(shared).map_err(|_| corrupt("prefix out of range"))?;
        if shared > previous.len() {
            return Err(corrupt("shared prefix longer than the previous term"));
        }
        let suffix_len =
            read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated suffix length"))?;
        let suffix_len =
            usize::try_from(suffix_len).map_err(|_| corrupt("suffix length out of range"))?;
        let end = cursor
            .checked_add(suffix_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| corrupt("suffix runs past the section"))?;

        let mut term_bytes = previous[..shared].to_vec();
        term_bytes.extend_from_slice(&bytes[cursor..end]);
        cursor = end;

        let gap = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated offset"))?;
        offset = offset
            .checked_add(gap)
            .ok_or_else(|| corrupt("offset overflow"))?;
        let doc_freq = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated freq"))?;
        let doc_freq = u32::try_from(doc_freq).map_err(|_| corrupt("freq out of range"))?;

        let term = String::from_utf8(term_bytes.clone())
            .map_err(|_| corrupt("term is not valid UTF-8"))?;
        // Length is filled in once the next offset is known.
        entries.push(TermEntry {
            term,
            offset,
            length: 0,
            doc_freq,
        });
        previous = term_bytes;
    }

    for index in 0..entries.len() {
        let next = entries
            .get(index + 1)
            .map_or(postings_len, |entry| entry.offset);
        entries[index].length = next.saturating_sub(entries[index].offset);
    }
    Ok(entries)
}

fn read_hosts(bytes: &[u8]) -> Result<Vec<String>, SegmentError> {
    let corrupt = |what: &str| SegmentError::Corrupt(format!("host table: {what}"));

    let mut cursor = 0usize;
    let count = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated host count"))?;
    let count = usize::try_from(count).map_err(|_| corrupt("implausible host count"))?;
    if count > bytes.len() + 1 {
        return Err(corrupt("host count exceeds the section size"));
    }

    let mut hosts = Vec::with_capacity(count);
    let mut previous = Vec::<u8>::new();
    for _ in 0..count {
        let shared = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated prefix"))?;
        let shared = usize::try_from(shared).map_err(|_| corrupt("prefix out of range"))?;
        if shared > previous.len() {
            return Err(corrupt("shared prefix longer than the previous host"));
        }
        let suffix_len =
            read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated suffix length"))?;
        let suffix_len =
            usize::try_from(suffix_len).map_err(|_| corrupt("suffix length out of range"))?;
        let end = cursor
            .checked_add(suffix_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| corrupt("suffix runs past the section"))?;

        let mut host = previous[..shared].to_vec();
        host.extend_from_slice(&bytes[cursor..end]);
        cursor = end;
        hosts.push(String::from_utf8(host.clone()).map_err(|_| corrupt("host is not UTF-8"))?);
        previous = host;
    }
    Ok(hosts)
}

fn read_doc_table(bytes: &[u8]) -> Result<Vec<DocEntry>, SegmentError> {
    let corrupt = |what: &str| SegmentError::Corrupt(format!("doc table: {what}"));

    let mut cursor = 0usize;
    let count = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated doc count"))?;
    let count = usize::try_from(count).map_err(|_| corrupt("implausible doc count"))?;
    if count > bytes.len() + 1 {
        return Err(corrupt("doc count exceeds the section size"));
    }

    let mut docs = Vec::with_capacity(count);
    for _ in 0..count {
        let crawl_doc = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated id"))?;
        let crawl_doc = u32::try_from(crawl_doc).map_err(|_| corrupt("crawl id out of range"))?;
        let host = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated host"))?;
        let host = u32::try_from(host).map_err(|_| corrupt("host id out of range"))?;
        let mut lengths = FieldCounts::default();
        for field in 0..FIELD_COUNT {
            let length = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt("truncated len"))?;
            lengths.set_index(field, u32::try_from(length).unwrap_or(u32::MAX));
        }
        let mut read_small = |what: &'static str| -> Result<u32, SegmentError> {
            let value = read_varint(bytes, &mut cursor).ok_or_else(|| corrupt(what))?;
            Ok(u32::try_from(value).unwrap_or(u32::MAX))
        };
        let quality = DocQuality {
            text_ratio: from_thousandths(read_small("truncated text ratio")?),
            link_density: from_thousandths(read_small("truncated link density")?),
            scripts: read_small("truncated script count")?,
        };
        docs.push(DocEntry {
            crawl_doc,
            host,
            lengths,
            quality,
        });
    }
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::{DocQuality, Document, SegmentBuilder, SegmentError, SegmentReader};
    use crate::fields::Field;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("uruk-seg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("segment.uruk")
    }

    fn corpus() -> Vec<(String, String, Vec<String>, String)> {
        vec![
            (
                "https://a.test/blog/clay-tablets".into(),
                "Clay tablets and the receipt".into(),
                vec!["The first ledgers".into()],
                "the scribes of uruk pressed reed into clay to count barley and sheep".into(),
            ),
            (
                "https://b.test/harbours".into(),
                "Deep harbours".into(),
                vec!["Sediment".into()],
                "a harbour is an argument with sediment because rivers drop silt".into(),
            ),
            (
                "https://c.test/uruk-again".into(),
                "More about Uruk".into(),
                vec![],
                "clay and reed again and again the scribes of uruk".into(),
            ),
        ]
    }

    fn build(path: &std::path::Path) -> SegmentBuilder {
        let mut builder = SegmentBuilder::new();
        for (index, (url, title, headings, body)) in corpus().into_iter().enumerate() {
            builder.add(&Document {
                crawl_doc: u32::try_from(index).unwrap() + 100,
                url: &url,
                title: &title,
                headings: &headings,
                body: &body,
                host: url.split('/').nth(2).unwrap_or_default(),
                quality: DocQuality {
                    text_ratio: 0.4,
                    link_density: 0.1,
                    scripts: 2,
                },
            });
        }
        builder.write(path).unwrap();
        builder
    }

    #[test]
    fn a_segment_round_trips() {
        let path = temp("round-trip");
        let builder = build(&path);

        let reader = SegmentReader::open(&path).unwrap();
        assert_eq!(reader.len(), builder.len());
        assert_eq!(reader.terms(), builder.terms.len());
    }

    #[test]
    fn postings_carry_documents_frequencies_and_positions() {
        let path = temp("postings");
        build(&path);
        let mut reader = SegmentReader::open(&path).unwrap();

        // "uruk" is in the body of documents 0 and 2, and in the URL of both.
        let postings = reader.postings("uruk").unwrap();
        assert_eq!(postings.iter().map(|p| p.doc).collect::<Vec<_>>(), [0, 2]);
        assert!(postings[0].counts.get(Field::Body) >= 1);
        assert!(!postings[0].positions.is_empty());

        // "scribes" appears once in each of documents 0 and 2.
        let scribes = reader.postings("scribes").unwrap();
        assert_eq!(scribes.len(), 2);
        assert_eq!(scribes[0].counts.get(Field::Body), 1);
    }

    #[test]
    fn repeated_terms_are_counted_not_duplicated() {
        let path = temp("repeats");
        build(&path);
        let mut reader = SegmentReader::open(&path).unwrap();

        // "again" occurs twice in document 2's body.
        let postings = reader.postings("again").unwrap();
        assert_eq!(
            postings.len(),
            1,
            "one posting per document, not per occurrence"
        );
        assert_eq!(postings[0].counts.get(Field::Body), 2);
        // The URL is /uruk-again, so the term is also in the Url field, and
        // every field's occurrences now carry a position.
        assert_eq!(postings[0].counts.get(Field::Url), 1);
        assert_eq!(postings[0].positions.len(), 3);
    }

    #[test]
    fn field_membership_is_recorded() {
        let path = temp("fields");
        build(&path);
        let mut reader = SegmentReader::open(&path).unwrap();

        // "harbours" is in the title and the URL but not the body, which reads
        // "a harbour is...".
        let postings = reader.postings("harbours").unwrap();
        assert_eq!(postings.len(), 1);
        assert_eq!(postings[0].counts.get(Field::Title), 1);
        assert_eq!(postings[0].counts.get(Field::Url), 1);
        assert_eq!(postings[0].counts.get(Field::Body), 0);
    }

    #[test]
    fn an_absent_term_returns_nothing_rather_than_failing() {
        let path = temp("absent");
        build(&path);
        let mut reader = SegmentReader::open(&path).unwrap();
        assert!(reader.postings("babylon").unwrap().is_empty());
        assert_eq!(reader.doc_frequency("babylon"), 0);
    }

    #[test]
    fn document_frequency_matches_the_posting_list() {
        let path = temp("df");
        build(&path);
        let mut reader = SegmentReader::open(&path).unwrap();
        for term in ["uruk", "clay", "sediment", "the"] {
            let expected = reader.postings(term).unwrap().len();
            assert_eq!(reader.doc_frequency(term) as usize, expected, "term {term}");
        }
    }

    #[test]
    fn the_doc_table_maps_back_to_the_crawl_store() {
        let path = temp("doctable");
        build(&path);
        let reader = SegmentReader::open(&path).unwrap();
        assert_eq!(reader.doc(0).unwrap().crawl_doc, 100);
        assert_eq!(reader.doc(2).unwrap().crawl_doc, 102);
        assert!(reader.doc(99).is_none());
        assert!(reader.doc(0).unwrap().lengths.get(Field::Body) > 0);
        // Quality signals survive the round trip, at thousandth precision.
        let quality = reader.doc(0).unwrap().quality;
        assert!(
            (quality.text_ratio - 0.4).abs() < 0.002,
            "text ratio: {}",
            quality.text_ratio
        );
        assert_eq!(quality.scripts, 2);
    }

    #[test]
    fn average_length_is_available_for_bm25() {
        let path = temp("avgdl");
        build(&path);
        let reader = SegmentReader::open(&path).unwrap();
        assert!(reader.average_length(Field::Body) > 5.0);
        assert!(reader.average_length(Field::Title) > 0.0);
    }

    #[test]
    fn front_coding_shrinks_a_dictionary_of_similar_terms() {
        // Sorted terms share long prefixes, which is the whole bet.
        let path = temp("frontcode");
        let mut builder = SegmentBuilder::new();
        let body = (0..500)
            .map(|i| format!("internationalisation{i:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        builder.add(&Document {
            crawl_doc: 0,
            url: "https://a.test/",
            title: "",
            headings: &[],
            body: &body,
            host: "a.test",
            quality: DocQuality::default(),
        });
        let manifest = builder.write(&path).unwrap();

        // 500 terms of 24 bytes is 12,000 bytes stored naively.
        assert!(
            manifest.bytes_dictionary < 6_000,
            "front coding saved nothing: {} bytes for {} terms",
            manifest.bytes_dictionary,
            manifest.terms
        );
        // And it still reads back correctly.
        let reader = SegmentReader::open(&path).unwrap();
        assert_eq!(reader.doc_frequency("internationalisation0499"), 1);
    }

    #[test]
    fn hosts_round_trip_and_are_searchable() {
        let path = temp("hosts");
        build(&path);
        let reader = SegmentReader::open(&path).unwrap();

        // The fixture has three documents on three different hosts.
        assert_eq!(reader.host_count(), 3);
        let id = reader.host_id("b.test").expect("b.test should be present");
        assert_eq!(reader.host_name(id), Some("b.test"));

        // Document 1 is the harbours page on b.test.
        assert_eq!(reader.doc(1).unwrap().host, id);
    }

    #[test]
    fn a_host_lookup_is_case_insensitive_and_can_miss() {
        let path = temp("hostcase");
        build(&path);
        let reader = SegmentReader::open(&path).unwrap();
        assert!(reader.host_id("A.TEST").is_some());
        // A miss is how a `site:` query skips a whole segment.
        assert!(reader.host_id("nowhere.test").is_none());
    }

    #[test]
    fn every_document_resolves_to_a_host_name() {
        let path = temp("hostnames");
        build(&path);
        let reader = SegmentReader::open(&path).unwrap();
        let hosts: Vec<&str> = (0..3)
            .map(|doc| reader.host_name(reader.doc(doc).unwrap().host).unwrap())
            .collect();
        assert_eq!(hosts, ["a.test", "b.test", "c.test"]);
    }

    #[test]
    fn an_empty_segment_is_valid() {
        let path = temp("empty");
        SegmentBuilder::new().write(&path).unwrap();
        let mut reader = SegmentReader::open(&path).unwrap();
        assert!(reader.is_empty());
        assert_eq!(reader.terms(), 0);
        assert!(reader.postings("anything").unwrap().is_empty());
        assert!(reader.average_length(Field::Body).abs() < f64::EPSILON);
    }

    #[test]
    fn a_foreign_file_is_rejected() {
        let path = temp("magic");
        std::fs::write(&path, b"this is not a segment at all, not even close").unwrap();
        assert!(matches!(
            SegmentReader::open(&path),
            Err(SegmentError::BadMagic { .. })
        ));
    }

    #[test]
    fn a_truncated_segment_is_reported_rather_than_misread() {
        let path = temp("truncated");
        build(&path);
        let full = std::fs::read(&path).unwrap();
        // Cut the footer off: the offsets it holds are the only way in.
        std::fs::write(&path, &full[..full.len() - 20]).unwrap();
        assert!(SegmentReader::open(&path).is_err());
    }

    #[test]
    fn the_manifest_describes_the_segment() {
        let path = temp("manifest");
        let manifest = build(&path).write(&path).unwrap();
        assert_eq!(manifest.documents, 3);
        assert!(manifest.terms > 10);
        assert!(manifest.bytes_postings > 0);
        assert!(manifest.bytes_dictionary > 0);
        assert_eq!(
            manifest.bytes_total,
            std::fs::metadata(&path).unwrap().len(),
            "the manifest's byte total should match the file on disk"
        );
        // And it is readable as JSON beside the segment.
        let json = std::fs::read_to_string(path.with_extension("json")).unwrap();
        assert!(json.contains("\"documents\""));
    }

    #[test]
    fn title_terms_get_positions_too() {
        let path = temp("positions");
        build(&path);
        let mut reader = SegmentReader::open(&path).unwrap();
        // "deep" is only in the title of document 1. Without a position it
        // could never take part in a quoted phrase, so a page titled "Deep
        // harbours" would not answer a search for that exact phrase.
        let postings = reader.postings("deep").unwrap();
        assert_eq!(postings[0].counts.get(Field::Title), 1);
        assert!(
            !postings[0].positions.is_empty(),
            "a title term has no position"
        );
    }

    #[test]
    fn fields_are_far_enough_apart_that_a_phrase_cannot_straddle_them() {
        let path = temp("fieldgap");
        build(&path);
        let mut reader = SegmentReader::open(&path).unwrap();

        // "harbours" is in document 1's title; "sediment" is in its body. If
        // the position spaces ran together these could look adjacent.
        let title_term = reader.postings("harbours").unwrap()[0].positions[0];
        let body_term = reader.postings("sediment").unwrap()[0].positions[0];
        assert!(
            body_term.abs_diff(title_term) > 1,
            "title at {title_term} and body at {body_term} are adjacent"
        );
    }
}
