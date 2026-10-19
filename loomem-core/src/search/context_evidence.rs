//! Bounded, lossless source windows and validated transient evidence projections.
//! No memory is persisted or synthesized: excerpts are exact original byte slices.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::HybridSearchResult;

const MAX_CANDIDATES: usize = 60;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024;
const MAX_QUERY_BYTES: usize = 16 * 1024;
const MAX_WINDOWS: usize = 2048;
const WINDOW_CHARS: usize = 400;
const OVERLAP_CHARS: usize = 64;
const MAX_SELECTED_RECORDS: usize = 20;
const MAX_SELECTED_WINDOWS: usize = 4;
const EXCERPT_MARKER: &str = "[E6 source excerpts; original record id unchanged]";
const GAP_MARKER: &str = "\n[E6 omitted source gap]\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// Window zero is always included when this record is selected.
pub struct EvidenceRecord {
    pub record_index: usize,
    pub windows: Vec<EvidenceWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceWindow {
    pub window_index: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
/// Select up to four windows; source header window zero is added automatically.
pub struct EvidenceChoice {
    pub record_index: usize,
    pub window_indices: Vec<usize>,
}

/// Prepare complete overlapping source coverage; invalid resource bounds fail open.
#[must_use]
pub fn prepare(query: &str, candidates: &[HybridSearchResult]) -> Option<Vec<EvidenceRecord>> {
    if !within_bounds(query, candidates) {
        return None;
    }
    let records: Vec<_> = candidates
        .iter()
        .enumerate()
        .map(|(record_index, record)| EvidenceRecord {
            record_index,
            windows: windows(&record.content),
        })
        .collect();
    (records
        .iter()
        .map(|record| record.windows.len())
        .sum::<usize>()
        <= MAX_WINDOWS)
        .then_some(records)
}

/// Project validated selected records first, appending every other record unchanged.
/// Any invalid, duplicate, empty, or out-of-bounds selection returns exact input.
#[must_use]
pub fn apply(
    candidates: Vec<HybridSearchResult>,
    selections: &[EvidenceChoice],
) -> Vec<HybridSearchResult> {
    let Some(prepared) = prepare("", &candidates) else {
        return candidates;
    };
    if !validate_selection(&prepared, selections) {
        return candidates;
    }
    let selected: BTreeSet<_> = selections
        .iter()
        .map(|choice| choice.record_index)
        .collect();
    let mut output: Vec<_> = selections
        .iter()
        .map(|choice| {
            let mut record = candidates[choice.record_index].clone();
            let spans = merged_spans(&prepared[choice.record_index], &choice.window_indices);
            if spans != [(0, record.content.len())] {
                record.content = render(&record.content, &spans);
            }
            record
        })
        .collect();
    output.extend(
        candidates
            .into_iter()
            .enumerate()
            .filter_map(|(index, record)| (!selected.contains(&index)).then_some(record)),
    );
    output
}

fn within_bounds(query: &str, candidates: &[HybridSearchResult]) -> bool {
    query.len() <= MAX_QUERY_BYTES
        && candidates.len() <= MAX_CANDIDATES
        && candidates
            .iter()
            .all(|record| record.content.len() <= MAX_RECORD_BYTES)
        && candidates
            .iter()
            .map(|record| record.content.len())
            .sum::<usize>()
            <= MAX_TOTAL_BYTES
}

fn windows(content: &str) -> Vec<EvidenceWindow> {
    let chars: Vec<_> = content.char_indices().collect();
    let mut output = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut end = (start + WINDOW_CHARS).min(chars.len());
        if end < chars.len() {
            end = whitespace_end(&chars, start, end);
        }
        let start_byte = chars[start].0;
        let end_byte = chars.get(end).map_or(content.len(), |item| item.0);
        output.push(EvidenceWindow {
            window_index: output.len(),
            start_byte,
            end_byte,
            content: content[start_byte..end_byte].to_string(),
        });
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(OVERLAP_CHARS).max(start + 1);
    }
    output
}

fn whitespace_end(chars: &[(usize, char)], start: usize, end: usize) -> usize {
    let lower = end
        .saturating_sub(OVERLAP_CHARS)
        .max(start + OVERLAP_CHARS + 1);
    (lower..end)
        .rev()
        .find(|&index| chars[index].1.is_whitespace())
        .unwrap_or(end)
}

/// Check model selections before applying or reporting source provenance.
#[must_use]
pub fn validate_selection(records: &[EvidenceRecord], choices: &[EvidenceChoice]) -> bool {
    if choices.is_empty() || choices.len() > MAX_SELECTED_RECORDS {
        return false;
    }
    let mut seen = BTreeSet::new();
    choices.iter().all(|choice| {
        let Some(record) = records.get(choice.record_index) else {
            return false;
        };
        seen.insert(choice.record_index) && valid_windows(record, &choice.window_indices)
    })
}

fn valid_windows(record: &EvidenceRecord, indices: &[usize]) -> bool {
    !indices.is_empty()
        && indices.len() <= MAX_SELECTED_WINDOWS
        && indices.iter().all(|&index| index < record.windows.len())
        && indices.iter().collect::<BTreeSet<_>>().len() == indices.len()
}

/// Exact original byte ranges, including automatic header window zero.
/// Returns `None` for empty, duplicate, or invalid model window indices.
#[must_use]
pub fn selected_spans(record: &EvidenceRecord, indices: &[usize]) -> Option<Vec<(usize, usize)>> {
    valid_windows(record, indices).then(|| merged_spans(record, indices))
}

fn merged_spans(record: &EvidenceRecord, indices: &[usize]) -> Vec<(usize, usize)> {
    let mut indices = indices.to_vec();
    indices.push(0);
    let mut spans: Vec<_> = indices
        .iter()
        .map(|&index| {
            let window = &record.windows[index];
            (window.start_byte, window.end_byte)
        })
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn render(source: &str, spans: &[(usize, usize)]) -> String {
    let excerpts: Vec<_> = spans
        .iter()
        .map(|&(start, end)| format!("[E6 source bytes {start}..{end}]\n{}", &source[start..end]))
        .collect();
    format!("{EXCERPT_MARKER}\n{}", excerpts.join(GAP_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: usize, content: &str) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: content.into(),
            user_id: "user".into(),
            app_id: "app".into(),
            level: 2,
            timestamp: 123,
            score: 0.42,
            bm25_score: 0.33,
            vector_score: 0.27,
            time_decay_factor: 0.9,
        }
    }

    fn choice(record_index: usize, window_indices: &[usize]) -> EvidenceChoice {
        EvidenceChoice {
            record_index,
            window_indices: window_indices.to_vec(),
        }
    }

    #[test]
    fn prepare_covers_unicode_source_exactly_with_bounded_overlap() {
        for content in [" 東京🙂żółć\n".repeat(200), "x".repeat(1600)] {
            let prepared = prepare("query", &[record(0, &content)]).unwrap();
            let windows = &prepared[0].windows;
            assert_eq!(windows.first().unwrap().start_byte, 0);
            assert_eq!(windows.last().unwrap().end_byte, content.len());
            for window in windows {
                assert_eq!(window.content, content[window.start_byte..window.end_byte]);
                assert!(window.content.chars().count() <= WINDOW_CHARS);
            }
            for pair in windows.windows(2) {
                assert!(pair[1].start_byte < pair[0].end_byte);
                assert!(pair[1].end_byte > pair[0].end_byte);
                assert_eq!(
                    content[pair[1].start_byte..pair[0].end_byte]
                        .chars()
                        .count(),
                    OVERLAP_CHARS
                );
            }
        }
    }

    #[test]
    fn projection_preserves_fields_and_unselected_records() {
        let input = vec![
            record(0, "other"),
            record(1, &"東京 preferences ".repeat(120)),
        ];
        let prepared = prepare("", &input).unwrap();
        let expected = &prepared[1].windows[2];
        let output = apply(input.clone(), &[choice(1, &[2])]);
        assert_eq!(output[0].id, "1");
        assert!(output[0].content.contains(EXCERPT_MARKER));
        assert!(output[0].content.ends_with(&expected.content));
        assert!(output[0].content.contains(&prepared[1].windows[0].content));
        assert!(validate_selection(&prepared, &[choice(1, &[2])]));
        assert_eq!(selected_spans(&prepared[1], &[2]).unwrap().len(), 2);
        assert!(selected_spans(&prepared[1], &[2, 2]).is_none());
        let mut restored = output[0].clone();
        restored.content = input[1].content.clone();
        assert_eq!(
            serde_json::to_value(restored).unwrap(),
            serde_json::to_value(&input[1]).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&output[1]).unwrap(),
            serde_json::to_value(&input[0]).unwrap()
        );
        assert_eq!(output.len(), input.len());
    }

    #[test]
    fn windows_render_in_source_order_and_merge_overlap() {
        let source = "abcdefghij ".repeat(200);
        let input = vec![record(0, &source)];
        let prepared = prepare("", &input).unwrap();
        let spans = merged_spans(&prepared[0], &[3, 0, 1]);
        assert_eq!(spans.len(), 2);
        let output = apply(input, &[choice(0, &[3, 0, 1])]);
        assert_eq!(output[0].content.matches(GAP_MARKER).count(), 1);
        assert!(output[0].content.contains(&source[spans[0].0..spans[0].1]));
        assert!(output[0].content.ends_with(&source[spans[1].0..spans[1].1]));
    }

    #[test]
    fn full_coverage_preserves_original_content_without_markers() {
        let input = vec![record(0, &"🙂".repeat(600)), record(1, "short preference")];
        let output = apply(input.clone(), &[choice(0, &[1]), choice(1, &[0])]);
        assert_eq!(
            serde_json::to_value(output).unwrap(),
            serde_json::to_value(input).unwrap()
        );
    }

    #[test]
    fn invalid_choices_return_exact_input() {
        let input = vec![record(0, "hello"), record(1, "world")];
        let cases = vec![
            vec![],
            vec![choice(2, &[0])],
            vec![choice(0, &[])],
            vec![choice(0, &[1])],
            vec![choice(0, &[0, 0])],
            vec![choice(0, &[0]), choice(0, &[0])],
            vec![choice(0, &[0, 1, 2, 3, 4])],
        ];
        for choices in cases {
            assert_eq!(
                serde_json::to_value(apply(input.clone(), &choices)).unwrap(),
                serde_json::to_value(&input).unwrap()
            );
        }
        assert!(serde_json::from_str::<EvidenceChoice>(
            r#"{"record_index":0,"window_indices":[0],"text":"invented"}"#
        )
        .is_err());
    }

    #[test]
    fn selection_caps_fail_open_and_record_order_follows_choices() {
        let input: Vec<_> = (0..21).map(|id| record(id, "source")).collect();
        let too_many: Vec<_> = (0..21).map(|id| choice(id, &[0])).collect();
        assert_eq!(
            serde_json::to_value(apply(input.clone(), &too_many)).unwrap(),
            serde_json::to_value(&input).unwrap()
        );
        let output = apply(input, &[choice(2, &[0]), choice(0, &[0])]);
        assert_eq!(
            output
                .iter()
                .map(|item| item.id.as_str())
                .take(4)
                .collect::<Vec<_>>(),
            ["2", "0", "1", "3"]
        );
        let oversized = vec![record(0, &"x".repeat(MAX_RECORD_BYTES + 1))];
        assert_eq!(
            serde_json::to_value(apply(oversized.clone(), &[choice(0, &[0])])).unwrap(),
            serde_json::to_value(oversized).unwrap()
        );
    }

    #[test]
    fn preparation_bounds_and_empty_sources_are_explicit() {
        assert!(prepare(&"q".repeat(MAX_QUERY_BYTES + 1), &[]).is_none());
        assert!(prepare("", &[record(0, &"x".repeat(MAX_RECORD_BYTES + 1))]).is_none());
        assert!(prepare("", &(0..61).map(|id| record(id, "x")).collect::<Vec<_>>()).is_none());
        assert!(prepare(
            "",
            &(0..33)
                .map(|id| record(id, &"x".repeat(MAX_RECORD_BYTES)))
                .collect::<Vec<_>>()
        )
        .is_none());
        let input = vec![record(0, "")];
        assert!(prepare("", &input).unwrap()[0].windows.is_empty());
        assert_eq!(apply(input, &[choice(0, &[0])])[0].content, "");
    }
}
