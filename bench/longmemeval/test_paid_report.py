"""paid_report: separate state counts, letter/stage money equal to the journal, tolerant reads."""
from decimal import Decimal
import json
from pathlib import Path
import unittest

from paid_arms import ArmOutcome, run_arm, write_arm_outcome
from paid_call_journal import JournalError, write_create_only
from paid_judge import judge_arm, write_verdict
from paid_report import build_report, write_report
from test_paid_arms import ArmHarness


class ReportTests(unittest.TestCase):
    def populate(self, h):
        """q01: F ok + judged correct, S technical_error, A not_started_budget;
        q02: zero-facts context, F ok + invalid_judge verdict, S pending."""
        directory = Path(h.campaign.directory)
        q1, q2 = h.context("h1-q01"), h.context("h1-q02", facts_per_session=0)
        # q02's history extracted nothing: the owner's zero_facts state (not a mere empty retrieval).
        q2.update(zero_facts=True, zero_facts_extraction=True, empty_retrieval=False)
        for ctx in (q1, q2):
            write_create_only(directory / "contexts" / f"{ctx['question_id']}.json", ctx)
        f1 = run_arm("facts_retry", q1, h.deps)
        write_arm_outcome(directory, f1)
        write_verdict(directory, judge_arm(h.deps, h.holdout, q1, "facts_retry", f1))
        write_arm_outcome(directory, ArmOutcome("h1-q01", "selective_originals", "technical_error", error="bad json"))
        write_arm_outcome(directory, ArmOutcome("h1-q01", "always_originals", "not_started_budget", error="sublimit A"))
        f2 = run_arm("facts_retry", q2, h.deps)
        write_arm_outcome(directory, f2)
        write_verdict(directory, {"question_id": "h1-q02", "arm": "facts_retry", "candidate": f2.answer,
                                  "verdict": {"valid": False, "status": None, "correct": None, "answer_span": None,
                                              "reason": None, "error": "not JSON"},
                                  "score": "invalid_judge", "request_id": "f" * 64, "usage": None, "outcome": "ok"})
        write_create_only(directory / "extraction" / "h1.json",
                          {"history_id": "h1", "sessions": [
                              {"session_id": "h1-s1", "source_id": "x", "fragment_ids": [], "fact_ids": ["a"],
                               "zero_facts": False, "requests": ["r1", "r2"], "proof_sha256": "0" * 64},
                              {"session_id": "h1-s2", "source_id": "y", "fragment_ids": [], "fact_ids": [],
                               "zero_facts": True, "requests": ["r3"], "proof_sha256": "0" * 64}],
                           "fingerprints": {"binary": "b" * 64}, "effective_config_sha256": "c" * 64})
        write_create_only(directory / "events" / "h1.h1-s2.zero_facts.json", {"history_id": "h1", "session_id": "h1-s2"})
        write_create_only(directory / "counts" / ("d" * 64 + ".json"),
                          {"stage": "diagnostic", "arm": "facts_retry", "question_id": "h1-q01", "model": "gpt-6-luna",
                           "input_tokens": 120, "r_tokens": 44608, "at": 1})
        return directory

    def test_states_are_counted_separately(self):
        h = ArmHarness(self)
        directory = self.populate(h)
        report = build_report(h.campaign, h.journal.state, directory, h.holdout)
        f, s, a = (report["per_arm"][arm] for arm in ("facts_retry", "selective_originals", "always_originals"))
        self.assertEqual((f["expected"], s["expected"], a["expected"]), (2, 2, 1))
        self.assertEqual((f["correct"], f["invalid_judge"], f["judged"], f["zero_facts"]), (1, 1, 2, 1))
        self.assertEqual((s["technical_error"], s["pending"], s["correct"]), (1, 1, 0))
        self.assertEqual((a["not_started_budget"], a["expected"], a["judged"]), (1, 1, 0))
        self.assertEqual(report["states"]["h1-q01"], {"facts_retry": "correct", "selective_originals": "technical_error",
                                                       "always_originals": "not_started_budget"})
        self.assertEqual(report["states"]["h1-q02"], {"facts_retry": "invalid_judge", "selective_originals": "pending"})
        self.assertEqual(report["zero_facts_questions"], ["h1-q02"])
        self.assertEqual((f["accuracy_over_expected"], a["accuracy_over_expected"]), ("0.5", "0"))
        self.assertEqual(report["pins"], {"gpt-6-luna": "gpt-6-luna", "gpt-6.2-astra": "gpt-6.2-astra"})
        self.assertEqual((report["mode"], report["limit_usd"], report["sublimits_usd"]["F"]), ("paid", "100", "30"))

    def test_letter_costs_equal_journal_sums(self):
        h = ArmHarness(self)
        directory = self.populate(h)
        report = build_report(h.campaign, h.journal.state, directory, h.holdout)
        state = h.journal.state
        committed = state.committed_by_letter()
        money = report["money"]
        for letter, row in money["by_letter"].items():
            intents = [i for i in state.intents.values() if i["letter"] == letter]
            self.assertEqual(row["calls"], len(intents))
            self.assertEqual(Decimal(row["reserved_max_usd"]), sum((Decimal(i["max_usd"]) for i in intents), Decimal(0)))
            actual = sum((Decimal(state.settled[r]["actual_usd"]) for r, i in state.intents.items()
                          if i["letter"] == letter and r in state.settled), Decimal(0))
            self.assertEqual(Decimal(row["actual_usd"]), actual)
            self.assertEqual(Decimal(row["actual_usd"]), committed[letter])  # all settled: committed == actual
        self.assertEqual((money["by_letter"]["F"]["calls"], money["by_letter"]["S"]["calls"]), (5, 0))
        self.assertEqual(money["by_stage"]["judge"]["calls"], 1)
        self.assertEqual(money["by_letter_stage"]["F:diagnostic"]["calls"], 2)
        self.assertEqual(Decimal(money["total_actual_usd"]), state.committed_total())
        self.assertEqual(Decimal(report["ledger"]["spent_usd"]), state.committed_total())
        self.assertEqual(money["blocking_request_ids"], [])
        row = money["by_letter"]["F"]
        self.assertEqual((row["rejected_4xx"], row["uncertain"], row["model_drift"], row["bound_breach"], row["length"]),
                         (0, 0, 0, 0, 0))

    def test_extraction_counts_probes_and_events(self):
        h = ArmHarness(self)
        directory = self.populate(h)
        report = build_report(h.campaign, h.journal.state, directory, h.holdout)
        extraction = report["extraction"]
        self.assertEqual((extraction["histories"], extraction["sessions"], extraction["zero_facts_sessions"]), (1, 2, 1))
        self.assertEqual(extraction["requests_per_session"], {"min": 1, "median": 1.5, "max": 2})
        self.assertEqual((extraction["halted"], extraction["fingerprints"], extraction["fingerprints_consistent"]),
                         (False, {"binary": "b" * 64}, True))
        self.assertEqual(extraction["effective_config_sha256"], ["c" * 64])
        self.assertEqual(report["zero_facts_events"], 1)
        self.assertEqual(report["input_tokens"], {"probes": 1, "input_tokens_total": 120, "by_stage": {"diagnostic": 1}})

    def test_missing_directories_and_no_holdout_are_tolerated(self):
        h = ArmHarness(self)
        report = build_report(h.campaign, h.journal.state, h.campaign.directory)
        self.assertEqual(report["per_arm"]["facts_retry"]["expected"], 0)
        self.assertEqual(report["extraction"]["sessions"], 0)
        self.assertEqual(report["extraction"]["requests_per_session"], {"min": None, "median": None, "max": None})
        self.assertEqual((report["input_tokens"]["probes"], report["pins"], report["states"]), (0, {}, {}))
        self.assertEqual(Decimal(report["money"]["total_reserved_max_usd"]), Decimal(0))
        directory = self.populate(h)
        blind = build_report(h.campaign, h.journal.state, directory)  # denominators from the files alone
        self.assertEqual(blind["per_arm"]["facts_retry"]["expected"], 2)
        self.assertEqual(blind["per_arm"]["always_originals"]["expected"], 1)
        json.dumps(blind, allow_nan=False)

    def test_write_report_is_create_only_and_numbered(self):
        h = ArmHarness(self)
        report = build_report(h.campaign, h.journal.state, h.campaign.directory, h.holdout, clock=h.clock)
        self.assertIn("generated_at", report)
        first = write_report(h.campaign, report)
        second = write_report(h.campaign, report)
        self.assertEqual((first.name, second.name), ("report-1.json", "report-2.json"))
        self.assertEqual(json.loads(first.read_text()), report)
        with self.assertRaises(JournalError):
            write_create_only(first, report)


if __name__ == "__main__":
    unittest.main()
