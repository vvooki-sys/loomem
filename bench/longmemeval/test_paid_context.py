"""paid_context: facts, exact-slice originals menu, evidence packing, caps, context hash.

Also hosts the synthetic ``/v1/search`` fixtures (functions only) that the arm,
judge and report tests import: a history from the miniature holdout rendered
to sources, fragment hits that are exact byte slices, and level-1 fact hits.
"""
import json
from pathlib import Path
import tempfile
import unittest
import uuid

import paid_context
from paid_context import (ContextError, build_context, byte_caps_ok, byte_size, context_sha256, facts_records,
                          originals_menu, pack_evidence)
from paid_holdout import verify_holdout
from paid_test_support import load_fixture_sheet, write_holdout


# -- shared synthetic search fixtures ---------------------------------------
def source_id_for(session_id):
    return str(uuid.uuid5(uuid.NAMESPACE_URL, "loomem-bench-fixture/" + session_id))


def render(session):
    return "\n".join(f"{turn['role']}: {turn['content']}" for turn in session["turns"])


def sources_for(history):
    return {source_id_for(s["session_id"]): render(s) for s in history["sessions"]}


def evidence(source_id, date, start=None, end=None, author=None):
    return {"author": author, "conversation_date": date, "event_date": None, "event_date_provenance": None,
            "source_id": source_id, "byte_start": start, "byte_end": end}


def fragment_hit(source_id, content, index, start, end, date, score=0.9):
    return {"id": f"{source_id}:f{index}", "content": content.encode("utf-8")[start:end].decode("utf-8"),
            "score": score, "metadata": {"level": 0, "source": "fragment"},
            "evidence": evidence(source_id, date, start, end, "user" if index == 0 else "assistant")}


def fact_hit(index, text, source_id, date, score=0.95):
    return {"id": str(uuid.uuid5(uuid.NAMESPACE_URL, f"fact/{source_id}/{index}")), "content": text,
            "score": score, "metadata": {"level": 1}, "evidence": evidence(source_id, date)}


def synthetic_search(history, facts_per_session=1, extra_fragments=0):
    """Rank order: facts first, then one fragment per turn of every session (engine-like)."""
    hits, sources = [], sources_for(history)
    for session in history["sessions"]:
        sid, date = source_id_for(session["session_id"]), session["conversation_date"]
        for i in range(facts_per_session):
            hits.append(fact_hit(i, f"Synthetic fact {i} about {session['session_id']}.", sid, date))
    for session in history["sessions"]:
        sid, date, content = source_id_for(session["session_id"]), session["conversation_date"], sources[source_id_for(session["session_id"])]
        offset = 0
        for index, line in enumerate(content.split("\n")):
            end = offset + len(line.encode("utf-8"))
            hits.append(fragment_hit(sid, content, index, offset, end, date, score=0.8 - 0.01 * index))
            offset = end + 1
    for k in range(extra_fragments):  # repeated slices of the first source under new fragment indexes
        sid = next(iter(sources))
        hits.append(fragment_hit(sid, sources[sid], 100 + k, 0, 10, history["sessions"][0]["conversation_date"], 0.1))
    return {"results": hits, "total": len(hits)}


def holdout_fixture(test):
    directory = tempfile.TemporaryDirectory()
    test.addCleanup(directory.cleanup)
    root, manifest_sha, always_sha = write_holdout(directory.name)
    return Path(directory.name), verify_holdout(root, manifest_sha, always_sha)


class ContextTests(unittest.TestCase):
    def setUp(self):
        self.root, self.holdout = holdout_fixture(self)
        self.caps = load_fixture_sheet(self.root).caps()
        self.history = self.holdout.histories["h1"]
        self.sources = sources_for(self.history)
        self.search = synthetic_search(self.history)
        self.view = self.holdout.execution_view("h1-q01")

    def test_facts_are_level_one_hits_in_rank_order_with_null_offsets(self):
        facts = facts_records(self.search, set(self.sources))
        self.assertEqual(len(facts), 2)
        self.assertEqual([f["record_id"] for f in facts], [h["id"] for h in self.search["results"][:2]])
        for fact in facts:
            self.assertEqual((fact["byte_start"], fact["byte_end"]), (None, None))
            self.assertIn(fact["source_id"], self.sources)
        with self.assertRaisesRegex(ContextError, "outside the history"):
            facts_records(self.search, set())

    def test_menu_is_exact_slices_in_rank_order(self):
        menu = originals_menu(self.search, self.sources, self.caps["ORIGINALS_MENU_CAP"])
        self.assertEqual(len(menu), 4)
        self.assertTrue(all(paid_context.FRAGMENT_ID.match(r["record_id"]) for r in menu))
        self.assertEqual([r["record_id"] for r in menu], [h["id"] for h in self.search["results"][2:]])
        for record in menu:
            expected = self.sources[record["source_id"]].encode("utf-8")[record["byte_start"]:record["byte_end"]]
            self.assertEqual(record["content"].encode("utf-8"), expected)

    def test_one_wrong_byte_in_a_fragment_is_a_context_error(self):
        search = json.loads(json.dumps(self.search))
        fragment = search["results"][2]
        fragment["content"] = fragment["content"][:-1] + ("!" if fragment["content"][-1] != "!" else "?")
        with self.assertRaisesRegex(ContextError, "differs from the stored byte slice"):
            originals_menu(search, self.sources, self.caps["ORIGINALS_MENU_CAP"])
        shifted = json.loads(json.dumps(self.search))
        shifted["results"][2]["evidence"]["byte_end"] += 1
        with self.assertRaises(ContextError):
            originals_menu(shifted, self.sources, self.caps["ORIGINALS_MENU_CAP"])
        foreign = json.loads(json.dumps(self.search))
        foreign["results"][2]["evidence"]["source_id"] = str(uuid.uuid4())
        with self.assertRaises(ContextError):
            originals_menu(foreign, self.sources, self.caps["ORIGINALS_MENU_CAP"])

    def test_menu_stops_before_cap_and_level_mismatch_fails(self):
        full = originals_menu(self.search, self.sources, self.caps["ORIGINALS_MENU_CAP"])
        two = byte_size(full[:2])
        self.assertEqual(originals_menu(self.search, self.sources, two), full[:2])
        self.assertEqual(originals_menu(self.search, self.sources, byte_size(full[:3]) - 1), full[:2])
        self.assertEqual(originals_menu(self.search, self.sources, byte_size(full[:3])), full[:3])
        bad = json.loads(json.dumps(self.search))
        bad["results"][2]["metadata"]["level"] = 1
        with self.assertRaisesRegex(ContextError, "reported with level"):
            originals_menu(bad, self.sources, self.caps["ORIGINALS_MENU_CAP"])

    def test_non_fragment_level_zero_hits_are_ignored(self):
        search = json.loads(json.dumps(self.search))
        sid = next(iter(self.sources))
        search["results"].insert(0, {"id": sid, "content": self.sources[sid], "score": 1.0,
                                     "metadata": {"level": 0}, "evidence": evidence(sid, "2025-07-10")})
        self.assertEqual(len(originals_menu(search, self.sources, self.caps["ORIGINALS_MENU_CAP"])), 4)
        self.assertEqual(len(facts_records(search, set(self.sources))), 2)

    def test_pack_evidence_never_evicts_facts(self):
        facts = facts_records(self.search, set(self.sources))
        menu = originals_menu(self.search, self.sources, self.caps["ORIGINALS_MENU_CAP"])
        facts_only = byte_size({"facts": facts, "originals": []})
        packed = pack_evidence(facts, menu, [r["record_id"] for r in menu], facts_only + 1)
        # Facts stay; every selected original that did not fit is rejected WITH its id.
        self.assertEqual((packed["facts"], packed["originals"], packed["rejected_ids"]),
                         (facts, [], [r["record_id"] for r in menu]))
        self.assertEqual(packed["bytes"], facts_only)
        with self.assertRaisesRegex(ContextError, "facts alone exceed"):
            pack_evidence(facts, menu, [], facts_only - 1)
        self.assertEqual(set(packed), {"facts", "originals", "rejected_ids", "bytes"})

    def test_pack_evidence_limits_selection_and_rejects_unknown_ids(self):
        facts = facts_records(self.search, set(self.sources))
        search = synthetic_search(self.history, extra_fragments=20)
        menu = originals_menu(search, self.sources, self.caps["ORIGINALS_MENU_CAP"])
        self.assertEqual(len(menu), 24)
        ids = [r["record_id"] for r in menu]
        selected = list(reversed(ids)) + ["nope:f1", ids[0]]
        packed = pack_evidence(facts, menu, selected, self.caps["EVIDENCE_CAP"])
        self.assertEqual([r["record_id"] for r in packed["originals"]], ids[:16])
        self.assertEqual(packed["rejected_ids"], ["nope:f1"] + ids[16:])
        everything = pack_evidence(facts, menu, ids, self.caps["EVIDENCE_CAP"], max_selected=None)
        self.assertEqual(len(everything["originals"]), 24)
        with self.assertRaises(ContextError):
            pack_evidence(facts, menu, "not-a-list", self.caps["EVIDENCE_CAP"])

    def test_pack_evidence_adds_originals_while_they_fit(self):
        facts = facts_records(self.search, set(self.sources))
        menu = originals_menu(self.search, self.sources, self.caps["ORIGINALS_MENU_CAP"])
        ids = [r["record_id"] for r in menu]
        cap = byte_size({"facts": facts, "originals": menu[:2]})
        packed = pack_evidence(facts, menu, ids, cap)
        self.assertEqual([r["record_id"] for r in packed["originals"]], ids[:2])
        self.assertLessEqual(packed["bytes"], cap)

    def test_byte_caps(self):
        caps = dict(self.caps)
        byte_caps_ok("diagnostic", {"facts": [], "question": "q"}, caps)
        with self.assertRaisesRegex(ContextError, "QUESTION_CAP"):
            byte_caps_ok("diagnostic", {"question": "x" * (caps["QUESTION_CAP"] + 1)}, caps)
        with self.assertRaisesRegex(ContextError, "ANSWER_CAP"):
            byte_caps_ok("audit", {"proposed_answer": "x" * (caps["ANSWER_CAP"] + 1)}, caps)
        with self.assertRaisesRegex(ContextError, "EVIDENCE_CAP"):
            byte_caps_ok("answer", {"evidence": {"facts": ["x" * caps["EVIDENCE_CAP"]]}}, caps)
        byte_caps_ok("judge", {"source_conversations": ["x" * 100_000]}, caps)  # bounded by R, not a cap
        with self.assertRaisesRegex(ContextError, "caps lack"):
            byte_caps_ok("diagnostic", {"question": "q"}, {})

    def test_build_context_shape_and_hash(self):
        ctx = build_context(self.view, self.search, self.sources, self.caps, zero_facts=False)
        self.assertEqual(set(ctx), {"question_id", "history_id", "question", "question_date", "facts", "menu",
                                    "source_ids", "zero_facts", "context_sha256"})
        self.assertEqual((ctx["question_id"], ctx["zero_facts"], ctx["source_ids"]), ("h1-q01", False, sorted(self.sources)))
        self.assertEqual(ctx["context_sha256"], context_sha256(ctx))
        again = build_context(self.view, self.search, self.sources, self.caps, zero_facts=False)
        self.assertEqual(again["context_sha256"], ctx["context_sha256"])
        other = build_context(self.holdout.execution_view("h1-q02"), self.search, self.sources, self.caps, False)
        self.assertNotEqual(other["context_sha256"], ctx["context_sha256"])
        for marker in self.holdout.gold_markers("h1-q01"):
            self.assertNotIn(marker, json.dumps(ctx))

    def test_build_context_zero_facts_and_caps(self):
        empty = synthetic_search(self.history, facts_per_session=0)
        ctx = build_context(self.view, empty, self.sources, self.caps, zero_facts=False)
        self.assertEqual((ctx["facts"], ctx["zero_facts"], len(ctx["menu"])), ([], True, 4))
        tight = {**self.caps, "FACTS_JSON_CAP": 10}
        with self.assertRaisesRegex(ContextError, "FACTS_JSON_CAP"):
            build_context(self.view, self.search, self.sources, tight, zero_facts=False)
        with self.assertRaisesRegex(ContextError, "execution view"):
            build_context({**self.view, "reference_answer": "x"}, self.search, self.sources, self.caps, False)
        with self.assertRaises(ContextError):
            build_context(self.view, {"results": "nope"}, self.sources, self.caps, False)


if __name__ == "__main__":
    unittest.main()
