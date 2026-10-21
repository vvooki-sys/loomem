//! Turn detection: a deterministic parser for common transcript formats.
//!
//! Recognised at the start of a line (case-insensitive): `user:`,
//! `assistant:`, `human:`, `ai:`, `system:`, `[user]`, `[assistant]`,
//! `**user**:`, `**assistant**:`, `User —` / `Assistant —` (em or en dash),
//! and JSON-lines where every non-blank line is an object with `"role"` and
//! `"content"` keys. The role is normalised to lowercase; `human` maps to
//! `user` and `ai` to `assistant` so the reader sees one author vocabulary.
//! Other JSONL roles are kept as given (lowercased).
//!
//! Marker mode is trusted only when the text looks like a transcript: at
//! least two marker lines, at least one of them `user` (or `human`). A lone
//! `AI:` bullet inside prose, or `System:` lines in a pasted log, therefore
//! do not split the text or invent an author — it stays one unlabelled turn.
//!
//! Turn byte ranges tile the whole text: sorted, contiguous, starting at 0
//! and ending at `text.len()`. Whitespace between turns belongs to the
//! preceding turn; a whitespace-only prefix before the first marker is folded
//! into the first turn. Text with no markers is one turn with role `None`.

/// One detected turn: a byte range into the source text and its role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Turn {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) role: Option<String>,
}

/// Role words recognised as line-prefix markers.
const ROLE_WORDS: [&str; 5] = ["user", "assistant", "human", "ai", "system"];

/// Detect turns in `text`. Never fails; an unrecognised layout yields a
/// single unlabelled turn covering the whole text.
pub(super) fn detect_turns(text: &str) -> Vec<Turn> {
    let starts = jsonl_turn_starts(text).unwrap_or_else(|| marker_turn_starts(text));
    build_turns(text, starts)
}

/// Normalise a role label: lowercase, with `human` → `user` and
/// `ai` → `assistant`.
pub(super) fn normalize_role(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "human" => "user".to_string(),
        "ai" => "assistant".to_string(),
        _ => lower,
    }
}

/// Byte offset and role of every line that starts a turn, in order.
type TurnStarts = Vec<(usize, String)>;

/// Iterate `(line_start_offset, line_without_newline)` over `text`.
fn lines_with_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0usize;
    text.split_inclusive('\n').map(move |raw| {
        let start = offset;
        offset += raw.len();
        (start, raw.trim_end_matches('\n').trim_end_matches('\r'))
    })
}

/// JSONL mode: `Some(starts)` only when every non-blank line is a JSON
/// object carrying a string `"role"` and a `"content"` key, and at least one
/// such line exists. Ranges still refer to the raw line bytes.
fn jsonl_turn_starts(text: &str) -> Option<TurnStarts> {
    let mut starts = TurnStarts::new();
    for (offset, line) in lines_with_offsets(text) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        let object = value.as_object()?;
        let role = object.get("role")?.as_str()?;
        object.get("content")?;
        starts.push((offset, normalize_role(role)));
    }
    if starts.is_empty() {
        None
    } else {
        Some(starts)
    }
}

/// Minimum number of marker lines before line-prefix mode is trusted.
const MIN_MARKER_LINES: usize = 2;

/// Line-prefix mode: every line whose prefix is a recognised role marker
/// starts a turn — provided the markers form a plausible transcript (see
/// the module docs); otherwise no turn starts at all.
fn marker_turn_starts(text: &str) -> TurnStarts {
    let starts: TurnStarts = lines_with_offsets(text)
        .filter_map(|(offset, line)| marker_role(line).map(|role| (offset, role)))
        .collect();
    let plausible =
        starts.len() >= MIN_MARKER_LINES && starts.iter().any(|(_, role)| role == "user");
    if plausible {
        starts
    } else {
        TurnStarts::new()
    }
}

/// The normalised role when `line` begins with a recognised marker.
fn marker_role(line: &str) -> Option<String> {
    let word = if let Some(inner) = line.strip_prefix("**") {
        bold_marker_word(inner)?
    } else if let Some(inner) = line.strip_prefix('[') {
        inner.split_once(']')?.0
    } else {
        plain_marker_word(line)?
    };
    let lower = word.trim().to_ascii_lowercase();
    ROLE_WORDS
        .contains(&lower.as_str())
        .then(|| normalize_role(&lower))
}

/// `**role**:` or `**role:**` — the role word between the asterisks.
fn bold_marker_word(inner: &str) -> Option<&str> {
    let (word, rest) = inner.split_once("**")?;
    if let Some(stripped) = word.strip_suffix(':') {
        return Some(stripped);
    }
    rest.starts_with(':').then_some(word)
}

/// `role:` or `Role —` — the leading ASCII word when a colon or dash follows.
fn plain_marker_word(line: &str) -> Option<&str> {
    let word_len = line.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
    let (word, rest) = line.split_at(word_len);
    (rest.starts_with(':') || dash_follows(rest)).then_some(word)
}

/// `User — text` / `Assistant – text`: an em or en dash after the role word,
/// optionally separated by spaces.
fn dash_follows(rest: &str) -> bool {
    let trimmed = rest.trim_start_matches(' ');
    trimmed.starts_with('\u{2014}') || trimmed.starts_with('\u{2013}')
}

/// Turn the sorted start offsets into tiling ranges.
fn build_turns(text: &str, starts: TurnStarts) -> Vec<Turn> {
    let len = text.len();
    let Some((first_offset, _)) = starts.first() else {
        return vec![Turn {
            start: 0,
            end: len,
            role: None,
        }];
    };
    let mut turns = Vec::with_capacity(starts.len() + 1);
    let leading = text.get(..*first_offset).unwrap_or("");
    if *first_offset > 0 && !leading.trim().is_empty() {
        turns.push(Turn {
            start: 0,
            end: *first_offset,
            role: None,
        });
    }
    for (i, (offset, role)) in starts.iter().enumerate() {
        let start = if turns.is_empty() { 0 } else { *offset };
        let end = starts.get(i + 1).map_or(len, |(next, _)| *next);
        turns.push(Turn {
            start,
            end,
            role: Some(role.clone()),
        });
    }
    turns
}
