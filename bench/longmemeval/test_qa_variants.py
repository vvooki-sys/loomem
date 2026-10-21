"""Synthetic selection checks; tokenizer integration belongs to qa_context tests."""
import unittest

from qa_variants import candidate_limit, neighbor_request_ids, variant_records


def source(identity, turn, content, session="synthetic"):
    return {"record_id": identity, "content": content, "session_id": session, "turn_index": turn}


def provenance(records):
    return [{"record_id": r["record_id"], "kind": "source", "sources": [
        {"session_id": r["session_id"], "turn_index": r["turn_index"]}]} for r in records]


class VariantTests(unittest.TestCase):
    def test_candidate_limits(self):
        self.assertEqual(candidate_limit("baseline20"), 20)
        self.assertEqual(candidate_limit("expanded60"), 60)
        rows = [source(str(index), index, "Evidence " + str(index)) for index in range(60)]
        self.assertEqual(len(variant_records("baseline20", rows[:20])), 20)
        self.assertEqual(len(variant_records("expanded60", rows)), 60)
        with self.assertRaises(ValueError):
            variant_records("baseline20", rows)

    def test_neighbor_expansion_uses_only_adjacent_same_session(self):
        corpus = [source("prev", 0, "Wolę pociąg."), source("seed", 1, "Chyba że ponad 6 godzin."),
                  source("next", 2, "Then fly."), source("far", 3, "Not adjacent."),
                  source("foreign", 0, "Other session", "other")]
        selected = variant_records("expanded60_neighbors", [corpus[1]], corpus)
        self.assertEqual([r["record_id"] for r in selected], ["seed", "prev", "next"])
        self.assertEqual(neighbor_request_ids(["seed"], provenance(corpus)), ["prev", "next"])

    def test_neighbor_refuses_missing_or_changed_actual_text(self):
        row = source("seed", 0, "[REDACTED]")
        for corpus in (None, [], [source("seed", 0, "unsanitized original")]):
            with self.assertRaises(ValueError):
                variant_records("expanded60_neighbors", [row], corpus)

    def test_label_fields_dropped_and_sanitized_text_unchanged(self):
        row = source("seed", 0, '{"content":"[REDACTED]","role":"user"}')
        row.update(answer="SECRET", has_answer=True)
        result = variant_records("baseline20", [row])
        self.assertEqual(result, [{"record_id": "seed", "content": row["content"]}])

    def test_duplicate_identity_and_conflicts(self):
        row = source("a", 0, "Same sentence.")
        other = source("b", 1, "Same sentence.")
        selected = variant_records("baseline20", [row, row, other])
        self.assertEqual([r["record_id"] for r in selected], ["a", "b"])
        with self.assertRaises(ValueError):
            variant_records("baseline20", [row, dict(row, content="different")])

    def test_fetch_ids_exclude_seeds_and_duplicate_neighbors(self):
        rows = [source(str(index), index, "unused") for index in range(5)]
        self.assertEqual(neighbor_request_ids(["1", "3"], provenance(rows)), ["0", "2", "4"])
        self.assertEqual(neighbor_request_ids(["0"], provenance(rows)), ["1"])
        with self.assertRaises(ValueError):
            neighbor_request_ids(["missing"], provenance(rows))

    def test_ambiguous_provenance_rejected(self):
        rows = [source("a", 0, "unused"), source("b", 0, "unused")]
        with self.assertRaises(ValueError):
            neighbor_request_ids(["a"], provenance(rows))
        with self.assertRaises(ValueError):
            variant_records("expanded60_neighbors", [rows[0]], rows)


if __name__ == "__main__":
    unittest.main()
