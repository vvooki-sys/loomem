"""Phase robustness on the dry-run path: an interrupted history with zero_facts resumes without a
JournalError, and a failed engine start never leaves the engine un-closed."""
import io
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import paid_phases
from paid_fake_engine import FakeEngine, FakeEngineError
from paid_runner import main as _main
from test_paid_preflight import build_fixture, runner_env, tokenizer_available


class FlakyEngine(FakeEngine):
    """Raises on the second session of the first history start only (an interrupted run)."""
    failures_left = 1
    closed = 0

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.ingested = 0

    def ingest_session(self, session):
        self.ingested += 1
        if self.ingested == 2 and FlakyEngine.failures_left:
            FlakyEngine.failures_left -= 1
            raise FakeEngineError("engine crashed mid-history")
        return super().ingest_session(session)

    def close(self):
        FlakyEngine.closed += 1


class RefusingEngine(FakeEngine):
    closed = 0

    def start(self, work, reuse=False):
        raise FakeEngineError("engine refused to start")

    def close(self):
        RefusingEngine.closed += 1


def factory_for(cls):
    def outer(campaign):
        profile = campaign.profile("extraction")
        return lambda url, token: cls(url, token, profile=profile, top_k=60)
    return outer


class RunPhasesSelectionTests(unittest.TestCase):
    def test_control_calls_no_phase_function_and_all_skips_control(self):
        calls = []
        fakes = {name: (lambda ctx, name=name: calls.append(name)) for name in paid_phases.PHASES}
        self.assertNotIn("control", paid_phases.PHASES)
        with mock.patch.dict(paid_phases.PHASES, fakes):
            self.assertEqual(paid_phases.run_phases(object(), "control"), [])
            self.assertEqual(calls, [])
            self.assertEqual(paid_phases.run_phases(object(), "all"), ["extraction", "retrieval", "arms", "judge"])
        self.assertEqual(calls, ["extraction", "retrieval", "arms", "judge"])


class PhaseResumeTests(unittest.TestCase):
    def setUp(self):
        if not tokenizer_available():
            self.skipTest("o200k cache unavailable (TIKTOKEN_CACHE_DIR)")
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fixture = build_fixture(self.root)
        self.campaign_dir = self.root / "dry"
        patch = mock.patch("paid_preflight.MIN_FREE_BYTES", 1)
        patch.start()
        self.addCleanup(patch.stop)

    def dry_run(self, cls, *extra):
        with mock.patch("sys.stderr", io.StringIO()):
            return _main(["dry-run", *self.fixture.flags(self.campaign_dir), *extra],
                         factories={"engine": factory_for(cls)}, stdout=io.StringIO(), env=runner_env())

    def test_interrupted_zero_facts_history_resumes_without_journal_error(self):
        FlakyEngine.failures_left, FlakyEngine.closed = 1, 0
        code = self.dry_run(FlakyEngine, "--scenario", "extraction=empty_facts")
        self.assertEqual(code, 3, "the first run stops on the engine crash")
        events = sorted(p.name for p in (self.campaign_dir / "events").glob("*.zero_facts.json"))
        self.assertEqual(events, ["h1.h1-s1.zero_facts.json"])
        self.assertFalse((self.campaign_dir / "extraction" / "h1.json").exists())
        self.assertEqual(FlakyEngine.closed, 1, "the crashed engine was closed")
        code = self.dry_run(FlakyEngine, "--resume", "--scenario", "extraction=empty_facts")
        self.assertEqual(code, 0, "the resumed run redoes the history; the existing event is not a duplicate")
        self.assertTrue((self.campaign_dir / "extraction" / "h1.json").is_file())
        events = sorted(p.name for p in (self.campaign_dir / "events").glob("*.zero_facts.json"))
        self.assertEqual(events, ["h1.h1-s1.zero_facts.json", "h1.h1-s2.zero_facts.json"])
        self.assertTrue(any(p.name.startswith("h1.partial-") for p in (self.campaign_dir / "engine").iterdir()))

    def test_failed_engine_start_is_closed_and_stops(self):
        RefusingEngine.closed = 0
        code = self.dry_run(RefusingEngine)
        self.assertEqual(code, 3)
        self.assertEqual(RefusingEngine.closed, 1, "close() runs even when start() raised")


if __name__ == "__main__":
    unittest.main()
