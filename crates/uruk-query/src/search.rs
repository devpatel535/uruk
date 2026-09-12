//! Running a query against an index.
//!
//! The order of work matters, because each step is cheaper than the one after
//! it and exists to give the next one less to do:
//!
//! 1. **Skip whole segments.** A `site:` filter for a host a segment has never
//!    seen, or a term its dictionary does not contain, rules the segment out
//!    without reading a single posting.
//! 2. **Intersect.** Boolean AND is the default, so only documents holding
//!    every query term are candidates. Walking the posting lists together
//!    starting from the shortest keeps this proportional to the rarest term
//!    rather than the commonest.
//! 3. **Filter.** Exclusions, the `site:` filter, and then phrases, which are
//!    checked last because they are the only step needing positions.
//! 4. **Score.** Only what survives is scored, and every survivor gets a full
//!    explanation rather than a number.
//!
//! Nothing here loads the index. Posting lists for the query's terms are read
//! and dropped; the per-query allocation is proportional to those lists, which
//! is the discipline `RESEARCH.md` §5.5 argues for in place of the brief's
//! "release memory when the query finishes".

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::time::{Duration, Instant};

use uruk_index::build::IndexError;
use uruk_index::index::{DocRef, Index};
use uruk_index::postings::Posting;
use uruk_link::Authority;

use crate::parse::Query;
use crate::score::{CorpusStats, Explanation, Scorer, TermScore, Weights};

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("could not read the index: {0}")]
    Index(#[from] IndexError),
}

/// How to run a search.
#[derive(Debug, Clone)]
pub struct SearchOptions<'a> {
    /// Results to return. Ten, because that is the product.
    pub limit: usize,
    pub weights: Weights,
    /// Host standing from the link graph, if it has been computed.
    ///
    /// Optional on purpose: an index can be searched before `uruk link` has
    /// ever run, and it should rank by text alone rather than refuse. When it
    /// is absent every result's explanation says "not measured" rather than
    /// showing a zero that looks like a judgement.
    pub authority: Option<&'a Authority>,
}

impl Default for SearchOptions<'_> {
    fn default() -> Self {
        Self {
            limit: 10,
            weights: Weights::default(),
            authority: None,
        }
    }
}

/// One result.
#[derive(Debug, Clone)]
pub struct Hit {
    pub doc: DocRef,
    /// Where to find the page's text in the crawl store.
    pub crawl_doc: u32,
    pub score: f64,
    pub explanation: Explanation,
}

/// What a search found.
#[derive(Debug, Clone)]
pub struct Results {
    pub hits: Vec<Hit>,
    /// Documents that matched, before the top-`limit` cut.
    pub matched: usize,
    /// Posting lists read. The honest measure of what the query cost.
    pub lists_read: usize,
    pub elapsed: Duration,
}

/// Ordering wrapper: `f64` is not `Ord`, and a heap needs it to be.
#[derive(Debug)]
struct Ranked(Hit);

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // `total_cmp` rather than `partial_cmp`: a NaN score must not make the
        // heap's ordering inconsistent, which is undefined behaviour territory
        // for a sort and silently wrong results for a heap.
        self.0.score.total_cmp(&other.0.score).then_with(|| {
            // Ties broken by document, so results are stable run to run.
            other.0.doc.cmp(&self.0.doc)
        })
    }
}

/// Search an index.
pub fn search(
    index: &mut Index,
    query: &Query,
    options: &SearchOptions<'_>,
) -> Result<Results, QueryError> {
    let started = Instant::now();
    let mut results = Results {
        hits: Vec::new(),
        matched: 0,
        lists_read: 0,
        elapsed: Duration::ZERO,
    };

    if query.is_empty() || index.is_empty() {
        results.elapsed = started.elapsed();
        return Ok(results);
    }

    let terms = query.distinct_terms();
    // Document frequencies are global and come from the dictionaries, so this
    // costs no reads. A term nobody has means the AND can never be satisfied.
    let frequencies: Vec<u32> = terms.iter().map(|term| index.doc_frequency(term)).collect();
    if frequencies.contains(&0) {
        results.elapsed = started.elapsed();
        return Ok(results);
    }

    let stats = CorpusStats {
        documents: index.len(),
        average_lengths: std::array::from_fn(|i| {
            uruk_index::fields::Field::from_index(i)
                .map_or(0.0, |field| index.average_length(field))
        }),
    };
    let scorer = Scorer::new(stats, options.weights);

    // A min-heap of the best `limit` seen so far: pushing and popping the
    // smallest keeps memory at `limit` rather than at the number of matches.
    let mut best: BinaryHeap<Reverse<Ranked>> = BinaryHeap::new();

    for segment in 0..u16::try_from(index.segment_count()).unwrap_or(u16::MAX) {
        let found = search_segment(
            index,
            segment,
            query,
            &terms,
            &frequencies,
            &scorer,
            options.authority,
        )?;
        results.lists_read += found.lists_read;
        results.matched += found.hits.len();

        for hit in found.hits {
            if best.len() < options.limit {
                best.push(Reverse(Ranked(hit)));
            } else if best
                .peek()
                .is_some_and(|Reverse(worst)| hit.score > worst.0.score)
            {
                best.pop();
                best.push(Reverse(Ranked(hit)));
            }
        }
    }

    let mut hits: Vec<Hit> = best.into_iter().map(|Reverse(Ranked(hit))| hit).collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.doc.cmp(&b.doc)));
    results.hits = hits;
    results.elapsed = started.elapsed();
    Ok(results)
}

struct SegmentHits {
    hits: Vec<Hit>,
    lists_read: usize,
}

fn search_segment(
    index: &mut Index,
    segment: u16,
    query: &Query,
    terms: &[&str],
    frequencies: &[u32],
    scorer: &Scorer,
    authority: Option<&Authority>,
) -> Result<SegmentHits, QueryError> {
    let mut lists_read = 0;

    // A `site:` filter that this segment cannot satisfy skips it entirely,
    // before any posting list is touched.
    let host_filter = match &query.site {
        Some(host) => match index.host_id(segment, host) {
            Some(id) => Some(id),
            None => {
                return Ok(SegmentHits {
                    hits: Vec::new(),
                    lists_read: 0,
                });
            }
        },
        None => None,
    };

    // Read each required term's list for this segment.
    let mut lists: Vec<Vec<Posting>> = Vec::with_capacity(terms.len());
    for term in terms {
        let postings = index.segment_postings(segment, term)?;
        lists_read += 1;
        if postings.is_empty() {
            // AND: one absent term ends this segment.
            return Ok(SegmentHits {
                hits: Vec::new(),
                lists_read,
            });
        }
        lists.push(postings);
    }

    let candidates = intersect(&lists);
    if candidates.is_empty() {
        return Ok(SegmentHits {
            hits: Vec::new(),
            lists_read,
        });
    }

    // Exclusions. Read only now: if nothing matched, they cost nothing.
    let (excluded, exclusion_reads) = excluded_docs(index, segment, query)?;
    lists_read += exclusion_reads;

    let mut hits = Vec::new();
    for (doc, postings) in candidates {
        if excluded.contains(&doc) {
            continue;
        }
        let Some(entry) = index.doc(DocRef { segment, doc }) else {
            continue;
        };
        if host_filter.is_some_and(|host| entry.host != host) {
            continue;
        }

        // Phrases last: the only check that needs positions.
        if !satisfies_phrases(&query.phrases, terms, &postings) {
            continue;
        }

        let contributions: Vec<TermScore> = postings
            .iter()
            .zip(terms)
            .zip(frequencies)
            .map(|((posting, name), &df)| scorer.term(name, df, posting.counts, entry.lengths))
            .collect();

        let span = closest_span(&postings.iter().map(|p| &p.positions).collect::<Vec<_>>());
        // Authority is a property of the host, so it is looked up once per
        // document rather than per term, and only for documents that survived
        // every filter above.
        let standing = authority.map(|table| {
            index
                .host_name(DocRef { segment, doc })
                .map_or(0.0, |host| table.score(host))
        });
        let explanation = scorer.document(contributions, entry.quality, span, standing);

        hits.push(Hit {
            doc: DocRef { segment, doc },
            crawl_doc: entry.crawl_doc,
            score: explanation.total,
            explanation,
        });
    }

    Ok(SegmentHits { hits, lists_read })
}

/// Documents this segment must not return, and how many lists that cost.
///
/// Separate from the main pass so that nothing is read for exclusions until
/// something has actually matched.
fn excluded_docs(
    index: &mut Index,
    segment: u16,
    query: &Query,
) -> Result<(BTreeSet<u32>, usize), QueryError> {
    let mut excluded = BTreeSet::new();
    let mut lists_read = 0;

    for term in &query.excluded {
        let postings = index.segment_postings(segment, term)?;
        lists_read += 1;
        excluded.extend(postings.iter().map(|posting| posting.doc));
    }

    for phrase in &query.excluded_phrases {
        let mut phrase_lists = Vec::with_capacity(phrase.len());
        let mut complete = true;
        for term in phrase {
            let postings = index.segment_postings(segment, term)?;
            lists_read += 1;
            if postings.is_empty() {
                // A phrase missing a term cannot occur, so nothing is excluded.
                complete = false;
                break;
            }
            phrase_lists.push(postings);
        }
        if !complete {
            continue;
        }
        for (doc, _) in intersect(&phrase_lists) {
            if positions_for(&phrase_lists, doc).is_some_and(|found| phrase_matches(&found)) {
                excluded.insert(doc);
            }
        }
    }
    Ok((excluded, lists_read))
}

/// Does this document satisfy every phrase in the query?
///
/// `postings` is one posting per entry of `terms`, in the same order, which is
/// what the intersection produced.
fn satisfies_phrases(phrases: &[Vec<String>], terms: &[&str], postings: &[Posting]) -> bool {
    phrases.iter().all(|phrase| {
        let positions: Option<Vec<&Vec<u32>>> = phrase
            .iter()
            .map(|term| {
                terms
                    .iter()
                    .position(|candidate| *candidate == term.as_str())
                    .map(|at| &postings[at].positions)
            })
            .collect();
        positions.is_some_and(|found| phrase_matches(&found))
    })
}

/// Documents present in every list, with each list's posting for them.
///
/// An n-way walk rather than repeated set intersection: the lists are sorted
/// by document id, so all of them advance together and nothing is allocated
/// per candidate that is then thrown away.
fn intersect(lists: &[Vec<Posting>]) -> Vec<(u32, Vec<Posting>)> {
    if lists.is_empty() || lists.iter().any(Vec::is_empty) {
        return Vec::new();
    }
    let mut cursors = vec![0usize; lists.len()];
    let mut out = Vec::new();

    loop {
        // The largest document any list is currently at. Everything below it
        // can be skipped, because AND cannot be satisfied there.
        let mut target = 0u32;
        for (list, &cursor) in lists.iter().zip(&cursors) {
            match list.get(cursor) {
                Some(posting) => target = target.max(posting.doc),
                None => return out,
            }
        }

        let mut aligned = true;
        for (list, cursor) in lists.iter().zip(&mut cursors) {
            while list
                .get(*cursor)
                .is_some_and(|posting| posting.doc < target)
            {
                *cursor += 1;
            }
            match list.get(*cursor) {
                Some(posting) if posting.doc == target => {}
                Some(_) => aligned = false,
                None => return out,
            }
        }

        if aligned {
            let postings: Vec<Posting> = lists
                .iter()
                .zip(&cursors)
                .map(|(list, &cursor)| list[cursor].clone())
                .collect();
            out.push((target, postings));
            for cursor in &mut cursors {
                *cursor += 1;
            }
        }
    }
}

/// Body positions of each term in `doc`, if every term has some.
fn positions_for(lists: &[Vec<Posting>], doc: u32) -> Option<Vec<&Vec<u32>>> {
    lists
        .iter()
        .map(|list| {
            list.binary_search_by_key(&doc, |posting| posting.doc)
                .ok()
                .map(|at| &list[at].positions)
        })
        .collect()
}

/// Do these terms appear adjacently, in order, anywhere?
///
/// Walks occurrences of the first term and checks the rest follow at
/// successive positions. Each check is a binary search, so a common first term
/// costs a logarithmic probe per occurrence rather than a scan.
pub fn phrase_matches(positions: &[&Vec<u32>]) -> bool {
    let Some((first, rest)) = positions.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return !first.is_empty();
    }
    if positions.iter().any(|list| list.is_empty()) {
        return false;
    }

    first.iter().any(|&start| {
        rest.iter().enumerate().all(|(offset, list)| {
            let wanted = start.checked_add(u32::try_from(offset).unwrap_or(u32::MAX) + 1);
            wanted.is_some_and(|wanted| list.binary_search(&wanted).is_ok())
        })
    })
}

/// Width of the smallest window containing one occurrence of every term.
///
/// `None` when any term has no body positions — it matched in the title or the
/// URL, where positions are not stored, so proximity is not a question that
/// can be answered rather than one whose answer is zero.
pub fn closest_span(positions: &[&Vec<u32>]) -> Option<u32> {
    if positions.len() < 2 || positions.iter().any(|list| list.is_empty()) {
        return None;
    }

    // Merge every occurrence into one ordered stream tagged with its term,
    // then slide a window that holds at least one of each and keep the
    // narrowest. Linear in the number of occurrences.
    let mut merged: Vec<(u32, usize)> = Vec::new();
    for (term, list) in positions.iter().enumerate() {
        merged.extend(list.iter().map(|&position| (position, term)));
    }
    merged.sort_unstable();

    let mut seen = vec![0usize; positions.len()];
    let mut distinct = 0usize;
    let mut start = 0usize;
    let mut best = u32::MAX;

    for end in 0..merged.len() {
        if seen[merged[end].1] == 0 {
            distinct += 1;
        }
        seen[merged[end].1] += 1;

        while distinct == positions.len() {
            best = best.min(merged[end].0 - merged[start].0);
            seen[merged[start].1] -= 1;
            if seen[merged[start].1] == 0 {
                distinct -= 1;
            }
            start += 1;
        }
    }

    (best != u32::MAX).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::{closest_span, intersect, phrase_matches};
    use uruk_index::fields::Field;
    use uruk_index::postings::Posting;

    fn posting(doc: u32, positions: &[u32]) -> Posting {
        let mut posting = Posting::new(doc);
        posting
            .counts
            .set(Field::Body, u32::try_from(positions.len()).unwrap());
        posting.positions = positions.to_vec();
        posting
    }

    #[test]
    fn intersection_keeps_only_shared_documents() {
        let a = vec![
            posting(1, &[0]),
            posting(3, &[0]),
            posting(5, &[0]),
            posting(9, &[0]),
        ];
        let b = vec![posting(3, &[1]), posting(4, &[1]), posting(9, &[1])];
        let docs: Vec<u32> = intersect(&[a, b]).into_iter().map(|(doc, _)| doc).collect();
        assert_eq!(docs, [3, 9]);
    }

    #[test]
    fn intersection_handles_three_lists() {
        let a = vec![posting(1, &[0]), posting(2, &[0]), posting(3, &[0])];
        let b = vec![posting(2, &[0]), posting(3, &[0]), posting(4, &[0])];
        let c = vec![posting(3, &[0]), posting(5, &[0])];
        let docs: Vec<u32> = intersect(&[a, b, c])
            .into_iter()
            .map(|(doc, _)| doc)
            .collect();
        assert_eq!(docs, [3]);
    }

    #[test]
    fn intersection_returns_each_list_posting_for_the_document() {
        let a = vec![posting(7, &[1, 2])];
        let b = vec![posting(7, &[9])];
        let found = intersect(&[a, b]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1[0].positions, [1, 2]);
        assert_eq!(found[0].1[1].positions, [9]);
    }

    #[test]
    fn intersection_of_disjoint_or_empty_lists_is_empty() {
        let a = vec![posting(1, &[0])];
        let b = vec![posting(2, &[0])];
        assert!(intersect(&[a.clone(), b]).is_empty());
        assert!(intersect(&[a, Vec::new()]).is_empty());
        assert!(intersect(&[]).is_empty());
    }

    #[test]
    fn a_single_list_intersects_to_itself() {
        let a = vec![posting(1, &[0]), posting(4, &[0])];
        let docs: Vec<u32> = intersect(&[a]).into_iter().map(|(doc, _)| doc).collect();
        assert_eq!(docs, [1, 4]);
    }

    #[test]
    fn a_phrase_matches_only_when_terms_are_adjacent_and_in_order() {
        let clay = vec![5u32, 40];
        let tablets = vec![6u32, 99];
        assert!(phrase_matches(&[&clay, &tablets]), "5,6 is adjacent");

        // Reversed: "tablets clay" does not occur.
        assert!(!phrase_matches(&[&tablets, &clay]));
    }

    #[test]
    fn a_phrase_of_three_terms_needs_all_three_adjacent() {
        let a = vec![10u32];
        let b = vec![11u32];
        let c = vec![12u32];
        assert!(phrase_matches(&[&a, &b, &c]));

        let far = vec![20u32];
        assert!(!phrase_matches(&[&a, &b, &far]));
    }

    #[test]
    fn a_phrase_finds_a_later_occurrence() {
        // The first candidate start fails; the second works.
        let a = vec![1u32, 50];
        let b = vec![51u32];
        assert!(phrase_matches(&[&a, &b]));
    }

    #[test]
    fn a_one_term_phrase_matches_if_the_term_occurs() {
        let a = vec![3u32];
        assert!(phrase_matches(&[&a]));
        let empty = Vec::new();
        assert!(!phrase_matches(&[&empty]));
    }

    #[test]
    fn a_phrase_with_a_term_that_has_no_positions_does_not_match() {
        // It matched in the title, where positions are not stored.
        let a = vec![1u32];
        let none = Vec::new();
        assert!(!phrase_matches(&[&a, &none]));
    }

    #[test]
    fn the_closest_span_is_the_narrowest_window() {
        let a = vec![0u32, 100];
        let b = vec![95u32, 300];
        // The best pairing is 100 and 95, five apart.
        assert_eq!(closest_span(&[&a, &b]), Some(5));
    }

    #[test]
    fn adjacent_terms_have_a_span_of_one() {
        let a = vec![10u32];
        let b = vec![11u32];
        assert_eq!(closest_span(&[&a, &b]), Some(1));
    }

    #[test]
    fn the_span_of_three_terms_covers_all_of_them() {
        let a = vec![0u32, 50];
        let b = vec![52u32];
        let c = vec![51u32];
        // 50..52 covers one of each.
        assert_eq!(closest_span(&[&a, &b, &c]), Some(2));
    }

    #[test]
    fn a_span_needs_at_least_two_terms_with_positions() {
        let a = vec![1u32];
        let empty = Vec::new();
        assert_eq!(closest_span(&[&a]), None);
        assert_eq!(closest_span(&[&a, &empty]), None);
        assert_eq!(closest_span(&[]), None);
    }

    #[test]
    fn the_span_ignores_which_term_comes_first() {
        // Proximity is about closeness, not order; order is what phrases are for.
        let a = vec![20u32];
        let b = vec![18u32];
        assert_eq!(closest_span(&[&a, &b]), Some(2));
        assert_eq!(closest_span(&[&b, &a]), Some(2));
    }
}
