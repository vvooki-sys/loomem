"""Read-only holdout verification and the gold-blind execution view (plan §2/§5).

``verify_holdout`` refuses unless the manifest bytes match the pinned SHA-256
and every file the manifest lists (questions, histories, fix logs, reviews,
tooling) matches its recorded hash; the frozen always-originals list is pinned
by its own hash. Execution stages receive ``execution_view`` only (question_id,
history_id, question, question_date); gold fields are exposed solely through
``judge_view`` for the judge/audit stage. Nothing here writes to the holdout.
"""
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path

EXECUTION_FIELDS = ("question_id", "history_id", "question", "question_date")
GOLD_FIELDS = ("reference_answer", "required_answer_atoms", "equivalence_rule",
               "unanswerable_explanation", "category")
# Leak markers: free-text gold only; `category` is a label that may legitimately
# appear in configuration or prompt text, so it is a judge field but not a marker.
PRIVATE_FIELDS = ("reference_answer", "required_answer_atoms", "equivalence_rule",
                  "unanswerable_explanation", "evidence", "critical_risk", "author_notes")
HASH_SECTIONS = ("files_sha256", "fix_logs_sha256", "reviews_sha256", "tooling_sha256")
MANIFEST_REQUIRED = HASH_SECTIONS + ("histories", "questions", "ordered_question_ids", "question_to_history")


class HoldoutError(ValueError):
    """The holdout is not the pinned one; refuse any run."""


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def session_bytes_of(session):
    return len(json.dumps(session, ensure_ascii=False).encode("utf-8"))


@dataclass(frozen=True)
class Holdout:
    directory: Path
    manifest_sha256: str
    always50_sha256: str
    histories: dict
    questions: dict
    question_order: tuple
    always_ids: frozenset

    def execution_view(self, qid):
        """The only question fields that extraction/retrieval/arm stages may see."""
        question = self._question(qid)
        return {key: question[key] for key in EXECUTION_FIELDS}

    def judge_view(self, qid):
        """Gold fields; import this only from the judge/audit module."""
        question = self._question(qid)
        return {key: question[key] for key in GOLD_FIELDS}

    def gold_markers(self, qid):
        """Non-trivial gold strings used to scan non-judge requests for leaks.

        A gold string that occurs verbatim in the question's own source sessions
        (an ``evidence`` quote, an answer atom that is a name from the transcript)
        is source content, not a leak: the engine's extraction requests and the
        originals menu legitimately carry it. Such strings are not markers.
        """
        question = self._question(qid)
        # The scan is global (every non-judge request against every question's markers),
        # so a string present in ANY holdout session is source content for the scan.
        sources = [turn["content"] for h in self.histories.values() for s in h["sessions"] for turn in s["turns"]]
        markers = []
        for key in PRIVATE_FIELDS:
            value = question.get(key)
            for text in _strings(value):
                if len(text) >= 12 and not any(text in source for source in sources):
                    markers.append(text)
        return markers

    def session_bytes(self):
        return [session_bytes_of(s) for hid in sorted(self.histories)
                for s in self.histories[hid]["sessions"]]

    def sources_bytes_max(self):
        return max(len(json.dumps(h["sessions"], ensure_ascii=False).encode("utf-8"))
                   for h in self.histories.values())

    def _question(self, qid):
        if qid not in self.questions:
            raise HoldoutError(f"unknown question {qid!r}")
        return self.questions[qid]


def leaks_gold(text, markers):
    return [m for m in markers if m in text]


def _strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from _strings(item)
    elif isinstance(value, dict):
        for item in value.values():
            yield from _strings(item)


def _check_hashes(directory, manifest):
    for section in HASH_SECTIONS:
        table = manifest[section]
        if not isinstance(table, dict):
            raise HoldoutError(f"manifest {section} must be an object")
        for relative, expected in table.items():
            path = directory / relative
            if not path.is_file():
                raise HoldoutError(f"listed file missing: {relative}")
            actual = sha256_file(path)
            if actual != expected:
                raise HoldoutError(f"file changed since manifest: {relative}")


def _load_questions(directory, manifest):
    questions = {}
    for relative in manifest["files_sha256"]:
        if not relative.endswith("questions.jsonl"):
            continue
        for line in (directory / relative).read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            row = json.loads(line)
            qid = row.get("question_id")
            if qid in questions:
                raise HoldoutError(f"duplicate question {qid!r}")
            missing = [k for k in EXECUTION_FIELDS + GOLD_FIELDS if k not in row]
            if missing:
                raise HoldoutError(f"question {qid!r} lacks {missing}")
            questions[qid] = row
    return questions


def _load_histories(directory, manifest):
    histories = {}
    for relative in manifest["files_sha256"]:
        if "/histories/" not in relative:
            continue
        history = json.loads((directory / relative).read_text(encoding="utf-8"))
        hid = history.get("history_id")
        if hid in histories or not isinstance(history.get("sessions"), list):
            raise HoldoutError(f"history {hid!r} duplicated or without sessions")
        for session in history["sessions"]:
            if not {"session_id", "conversation_date", "turns"} <= set(session):
                raise HoldoutError(f"history {hid!r}: session shape")
        histories[hid] = history
    return histories


def _check_consistency(manifest, questions, histories):
    order = manifest["ordered_question_ids"]
    if not isinstance(order, list) or len(order) != len(set(order)):
        raise HoldoutError("ordered_question_ids must be a list of unique IDs")
    if set(order) != set(questions) or len(order) != manifest["questions"]:
        raise HoldoutError("question files disagree with ordered_question_ids / questions count")
    if len(histories) != manifest["histories"]:
        raise HoldoutError("history files disagree with the histories count")
    q2h = manifest["question_to_history"]
    for qid, row in questions.items():
        hid = row["history_id"]
        if q2h.get(qid) != hid or hid not in histories:
            raise HoldoutError(f"question {qid!r}: history mapping inconsistent")
        last = max(s["conversation_date"] for s in histories[hid]["sessions"])
        if row["question_date"] < last:
            raise HoldoutError(f"question {qid!r}: question_date precedes its last session")


def _load_always(directory, manifest, always50_sha256, questions):
    path = directory / "always_originals_50.json"
    if not path.is_file():
        raise HoldoutError("always_originals_50.json missing")
    actual = sha256_file(path)
    if actual != always50_sha256:
        raise HoldoutError(f"always_originals_50.json SHA-256 mismatch: got {actual}")
    listed = json.loads(path.read_text(encoding="utf-8"))["question_ids"]
    if len(listed) != len(set(listed)) or not set(listed) <= set(questions):
        raise HoldoutError("always-originals IDs must be unique holdout questions")
    pinned = manifest.get("always_originals_50")
    if pinned is not None and (pinned.get("sha256") != always50_sha256 or pinned.get("question_ids") != listed):
        raise HoldoutError("manifest's always_originals_50 entry disagrees with the file")
    return frozenset(listed)


def verify_holdout(directory, manifest_sha256, always50_sha256):
    directory = Path(directory)
    manifest_path = directory / "manifest.json"
    if not manifest_path.is_file():
        raise HoldoutError(f"manifest missing: {manifest_path}")
    actual = sha256_file(manifest_path)
    if actual != manifest_sha256:
        raise HoldoutError(f"manifest SHA-256 mismatch: expected {manifest_sha256}, got {actual}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    missing = [k for k in MANIFEST_REQUIRED if k not in manifest]
    if missing:
        raise HoldoutError(f"manifest lacks {missing}")
    _check_hashes(directory, manifest)
    questions = _load_questions(directory, manifest)
    histories = _load_histories(directory, manifest)
    _check_consistency(manifest, questions, histories)
    always = _load_always(directory, manifest, always50_sha256, questions)
    return Holdout(directory, actual, always50_sha256, histories, questions,
                   tuple(manifest["ordered_question_ids"]), always)
