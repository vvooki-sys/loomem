"""Evaluation-only provenance diagnostics. Labels must never enter generation."""
import argparse
import json
from pathlib import Path


def _coverage(expected, present, complete):
    if expected is None or not complete or not expected:
        return {"recall": None, "all_support_present": None}
    return {"recall": len(expected & present) / len(expected),
            "all_support_present": expected <= present}


def evidence_coverage(question, records):
    """Report session/turn provenance and full-source-turn coverage separately.

    Fact provenance does NOT prove the extracted text retains the answer. Only
    complete raw turns count towards full_source_turn coverage; these remain
    evidence-availability diagnostics, never semantic correctness/QA scores.
    Missing gold labels or any selected record without provenance means unknown.
    """
    sessions = question.get("answer_session_ids")
    expected_sessions = set(sessions) if isinstance(sessions, list) else None
    turns = question.get("haystack_sessions", [])
    ids = question.get("haystack_session_ids", [])
    expected_turns = set()
    original_content = {}
    labels_complete = len(turns) == len(ids)
    for session_id, session in zip(ids, turns):
        for index, turn in enumerate(session):
            original_content[(session_id, index)] = turn.get("content")
            if type(turn.get("has_answer")) is not bool:
                labels_complete = False
            elif turn["has_answer"]:
                expected_turns.add((session_id, index))
    if not labels_complete:
        expected_turns = None
    present_sessions, present_turns, raw_turns = set(), set(), set()
    complete = True
    for record in records:
        sources = record.get("sources", [])
        if not sources:
            complete = False
        for source in sources:
            session = source.get("session_id")
            index = source.get("turn_index")
            if not isinstance(session, str) or type(index) is not int or index < 0:
                complete = False
                continue
            if (session, index) not in original_content:
                complete = False
                continue
            present_sessions.add(session)
            present_turns.add((session, index))
            if (record.get("kind") == "source"
                    and record.get("content") == original_content[(session, index)]):
                raw_turns.add((session, index))
    return {"provenance_complete": complete,
            "session_provenance": _coverage(expected_sessions, present_sessions, complete),
            "turn_provenance": _coverage(expected_turns, present_turns, complete),
            "full_source_turn": _coverage(expected_turns, raw_turns, complete),
            "semantic_support": "not_measured"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--question", type=Path, required=True,
                        help="one evaluation question JSON object (labels allowed here only)")
    parser.add_argument("--records", type=Path, required=True,
                        help="JSON array of records actually selected for context")
    args = parser.parse_args()
    question = json.loads(args.question.read_text())
    records = json.loads(args.records.read_text())
    print(json.dumps(evidence_coverage(question, records), sort_keys=True))


if __name__ == "__main__":
    main()
