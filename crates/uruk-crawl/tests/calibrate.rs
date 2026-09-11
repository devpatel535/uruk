//! Calibration of the near-duplicate threshold, kept as a test.
//!
//! `DEFAULT_THRESHOLD` is a number derived from these measurements rather than
//! inherited from a paper written at a different scale, so the measurements
//! have to keep holding. This asserts the two properties the choice rests on:
//! realistic near-duplicates fall inside the threshold, and unrelated
//! documents stay far outside it.
//!
//! Run with `--nocapture` to see the distributions rather than just the pass.

use uruk_crawl::simhash::{DEFAULT_THRESHOLD, SeenFingerprints, distance, fingerprint};

const BASE: &str = "The scribes of Uruk were not writing poetry when they first pressed a reed \
into wet clay. They were counting sheep, and they needed the count to survive the walk from the \
pen to the temple storehouse. What they invented, without meaning to, was a way of making a \
promise outlive the person who made it. A tablet recorded that a quantity of barley had changed \
hands, and it did so in a form that could be checked later by someone who had not been present \
at the transaction. That is the whole idea, and every ledger written since is a footnote to it. \
The marks themselves began as pictures and became wedges, because a wedge is what a cut reed \
leaves in clay when you press rather than drag it. Dragging tears the surface; pressing does \
not. So the shape of the writing was decided by the material, as the shape of writing usually \
is. The tablets were not fired on purpose. Most of what survives was baked by accident when a \
building burned down, which means the archive we have is a record of disasters rather than of \
importance. The ordinary tablets, the ones nobody thought worth keeping, dried in the sun and \
dissolved in the next rain. We read the fires and not the libraries. None of this was literature \
and none of it was meant to last. It lasted anyway, which is the argument for writing things \
down in a form that does not depend on anyone remembering to keep them.";

const OTHER: &str = "Rust's borrow checker is a proof system wearing the clothes of a compiler \
pass. It does not observe your program running and conclude that nothing went wrong; it refuses \
to compile programs for which it cannot construct an argument that nothing can go wrong. The \
distinction matters because it explains the frustration. A borrow error is not a report of a bug \
that occurred. It is a statement that the compiler could not find a proof, which sometimes means \
there is no proof and sometimes means the proof is beyond the system's vocabulary. Lifetimes are \
that vocabulary. They are not durations and they are not scopes, though they line up with scopes \
often enough to make the confusion durable. A lifetime is a region of code over which a reference \
must remain valid, and the checker's job is to show that the referent outlives the region. When \
people say they are fighting the borrow checker, what they usually mean is that they have a \
design in mind whose safety argument they have not yet written down, and the compiler is asking \
them to write it down. That is a real cost and it is paid up front, which is unusual and is why \
it feels expensive even when it is cheap.";

fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Distance after replacing the nth distinct long word with a nonsense token.
fn edit_distances(text: &str, edits: usize) -> Vec<u32> {
    let base = fingerprint(text);
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for word in words(text) {
        let clean: String = word.chars().filter(|c| c.is_alphanumeric()).collect();
        if clean.len() < 5 || !seen.insert(clean.clone()) {
            continue;
        }
        let edited = text.replacen(&clean, "zzqqx", 1);
        out.push(distance(base, fingerprint(&edited)));
        if out.len() >= edits {
            break;
        }
    }
    out
}

struct Spread {
    median: u32,
    max: u32,
    min: u32,
}

fn summarise(label: &str, mut values: Vec<u32>) -> Spread {
    values.sort_unstable();
    let n = values.len();
    // Integer percentiles: no float casts, and exact for the sizes here.
    let pct = |percent: usize| values[(n - 1) * percent / 100];
    println!(
        "{label:<34} n={n:3}  min={:2}  p50={:2}  p90={:2}  p95={:2}  max={:2}",
        values[0],
        pct(50),
        pct(90),
        pct(95),
        values[n - 1]
    );
    Spread {
        median: pct(50),
        max: values[n - 1],
        min: values[0],
    }
}

/// Unrelated documents must stay far outside the threshold. This is the
/// assertion that protects against silently discarding genuine content, which
/// is the failure mode that cannot be noticed after the fact.
#[test]
fn unrelated_documents_are_nowhere_near_the_threshold() {
    let mut unrelated = Vec::new();
    for i in 0..40 {
        let a = format!("{BASE} Variation {i} appends a distinct closing paragraph about {i}.");
        let b = format!("{OTHER} Variation {i} appends a distinct closing paragraph about {i}.");
        unrelated.push(distance(fingerprint(&a), fingerprint(&b)));
    }
    let spread = summarise("unrelated documents", unrelated);
    assert!(
        spread.min >= DEFAULT_THRESHOLD * 4,
        "unrelated documents came within {} bits of each other, but the threshold is {DEFAULT_THRESHOLD}",
        spread.min
    );
}

/// The shapes a real mirror, reprint or syndicated copy actually takes must
/// all land inside the threshold.
#[test]
fn realistic_near_duplicates_are_caught() {
    let base = fingerprint(BASE);
    let cases = [
        ("byte-identical", BASE.to_owned()),
        (
            "navigation header added",
            format!("Home About Archive Subscribe Contact\n{BASE}"),
        ),
        (
            "footer added",
            format!("{BASE}\nCopyright 2026. All rights reserved. Privacy policy."),
        ),
        (
            "truncated 1%",
            words(BASE)[..(words(BASE).len() * 99 / 100)].join(" "),
        ),
        (
            "truncated 5%",
            words(BASE)[..(words(BASE).len() * 95 / 100)].join(" "),
        ),
    ];

    let mut seen = SeenFingerprints::new();
    seen.insert(base);
    for (label, variant) in &cases {
        let moved = distance(base, fingerprint(variant));
        println!("{label:<34} {moved:2} bits");
        assert!(
            moved <= DEFAULT_THRESHOLD,
            "{label} moved {moved} bits, past the {DEFAULT_THRESHOLD}-bit threshold"
        );
        assert!(
            seen.find_near(fingerprint(variant)).is_some(),
            "{label} was not recognised"
        );
    }
}

#[test]
fn a_single_word_edit_usually_stays_inside_the_threshold() {
    let long = summarise("1-word edit, 255-word prose", edit_distances(BASE, 60));
    let short = summarise("1-word edit, 200-word prose", edit_distances(OTHER, 60));

    for spread in [&long, &short] {
        assert!(
            spread.median <= DEFAULT_THRESHOLD,
            "the median one-word edit moved {} bits, past the {DEFAULT_THRESHOLD}-bit threshold",
            spread.median
        );
        // The tail is real and is documented rather than wished away: what
        // matters is that it stays far below where unrelated documents sit.
        assert!(
            spread.max < DEFAULT_THRESHOLD * 4,
            "tail of {} bits is too long",
            spread.max
        );
    }
}

#[test]
fn report() {
    println!(
        "\nBASE is {} words, OTHER is {} words\n",
        words(BASE).len(),
        words(OTHER).len()
    );

    // Truncations and additions: the shapes a mirror or a reprint actually takes.
    let base = fingerprint(BASE);
    let mut structural = Vec::new();
    for keep_percent in [99usize, 97, 95, 90] {
        let n = words(BASE).len() * keep_percent / 100;
        let shortened = words(BASE)[..n].join(" ");
        structural.push(distance(base, fingerprint(&shortened)));
    }
    println!("truncation 1/3/5/10%:              {structural:?}");

    let with_header = format!("Home About Archive Subscribe Contact\n{BASE}");
    let with_footer = format!("{BASE}\nCopyright 2026. All rights reserved. Privacy policy.");
    println!(
        "nav header added:                  {}",
        distance(base, fingerprint(&with_header))
    );
    println!(
        "footer added:                      {}",
        distance(base, fingerprint(&with_footer))
    );
    println!(
        "byte-identical:                    {}",
        distance(base, fingerprint(BASE))
    );

    // The number that must stay large: unrelated documents.
    println!(
        "unrelated document:                {}",
        distance(base, fingerprint(OTHER))
    );

    println!();
}
