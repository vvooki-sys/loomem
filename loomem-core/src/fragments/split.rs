//! Size splitting: cut a turn that exceeds the token budget into pieces.
//!
//! A piece over budget is bisected at the boundary closest to its byte
//! midpoint, preferring (in order) paragraph breaks (`"\n\n"`), sentence
//! ends (`". "`, `"? "`, `"! "`, `"\n"`), whitespace, and finally a hard cut
//! on a char boundary. Both halves are re-examined with the full preference
//! order, so a long paragraph inside a multi-paragraph turn is split at
//! sentence ends while the other paragraphs keep their natural boundaries.
//! Whitespace following a boundary belongs to the preceding piece, so a
//! piece produced by a soft cut starts at a non-whitespace char.
//!
//! Every cut is a valid UTF-8 char boundary: delimiter cuts land after ASCII
//! delimiters or whole whitespace chars, and the hard cut searches outward
//! from the midpoint for `is_char_boundary`.

use super::{FragmentError, TokenCounter};

const PARAGRAPH_DELIMS: [&str; 1] = ["\n\n"];
const SENTENCE_DELIMS: [&str; 4] = [". ", "? ", "! ", "\n"];

/// Split `text[start..end]` into consecutive byte ranges whose text each
/// fits `max_tokens` (as measured by `counter`). Ranges tile `[start, end)`
/// in order. A piece that cannot be cut any further (a single char) is
/// emitted as-is even when it exceeds the budget.
pub(super) fn split_to_budget(
    text: &str,
    start: usize,
    end: usize,
    max_tokens: usize,
    counter: &dyn TokenCounter,
) -> Result<Vec<(usize, usize)>, FragmentError> {
    let mut out = Vec::new();
    let mut stack = vec![(start, end)];
    while let Some((s, e)) = stack.pop() {
        let piece = text.get(s..e).ok_or_else(|| {
            FragmentError::Invariant(format!("range {s}..{e} is not on char boundaries"))
        })?;
        if counter.count(piece)? <= max_tokens {
            out.push((s, e));
            continue;
        }
        match split_point(piece) {
            Some(p) => {
                stack.push((s + p, e));
                stack.push((s, s + p));
            }
            None => out.push((s, e)),
        }
    }
    Ok(out)
}

/// Best cut offset inside `piece` (exclusive of 0 and `piece.len()`), or
/// `None` when the piece holds fewer than two chars.
///
/// The soft tiers keep at least one non-whitespace char in the left piece,
/// so a cut never strands leading whitespace on its own when a better cut
/// exists. The hard tier has no such preference — it must always be able to
/// shrink the piece — so a whitespace-only fragment can arise only from a
/// hard cut of a piece padded with whitespace.
fn split_point(piece: &str) -> Option<usize> {
    let mid = piece.len() / 2;
    let min = piece.len() - piece.trim_start().len() + 1;
    let tiers = [
        delimiter_candidates(piece, &PARAGRAPH_DELIMS),
        delimiter_candidates(piece, &SENTENCE_DELIMS),
        whitespace_candidates(piece),
    ];
    for candidates in &tiers {
        if let Some(p) = nearest(candidates, mid, min, piece.len()) {
            return Some(p);
        }
    }
    hard_split(piece, mid)
}

/// Offsets right after each delimiter occurrence, extended over any
/// following whitespace.
fn delimiter_candidates(piece: &str, delims: &[&str]) -> Vec<usize> {
    let mut out: Vec<usize> = delims
        .iter()
        .flat_map(|d| piece.match_indices(d).map(|(i, m)| i + m.len()))
        .map(|p| skip_whitespace(piece, p))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Offsets of every non-whitespace char that follows a whitespace char.
fn whitespace_candidates(piece: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut prev_ws = false;
    for (i, c) in piece.char_indices() {
        if prev_ws && !c.is_whitespace() {
            out.push(i);
        }
        prev_ws = c.is_whitespace();
    }
    out
}

/// First offset at or after `p` that is not whitespace (or `piece.len()`).
fn skip_whitespace(piece: &str, p: usize) -> usize {
    piece
        .get(p..)
        .map_or(p, |rest| p + (rest.len() - rest.trim_start().len()))
}

/// The candidate in `[min, len)` closest to `mid`; ties go to the earlier
/// offset (candidates are sorted ascending).
fn nearest(candidates: &[usize], mid: usize, min: usize, len: usize) -> Option<usize> {
    candidates
        .iter()
        .copied()
        .filter(|&p| p >= min && p < len)
        .min_by_key(|&p| p.abs_diff(mid))
}

/// Char boundary in `[1, len)` closest to `mid`, searching upward first.
fn hard_split(piece: &str, mid: usize) -> Option<usize> {
    let len = piece.len();
    let mut up = mid.max(1);
    while up < len && !piece.is_char_boundary(up) {
        up += 1;
    }
    if up < len {
        return Some(up);
    }
    let mut down = mid.min(len);
    while down > 1 && !piece.is_char_boundary(down) {
        down -= 1;
    }
    (down >= 1 && down < len && piece.is_char_boundary(down)).then_some(down)
}
