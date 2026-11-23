"""The live engine probe with an injected engine (no binary): evaluation build, prompt size, pin."""
import json
import os
import tempfile
import unittest
from pathlib import Path
from urllib.request import Request, urlopen

from paid_preflight_engine import PROBE_SESSION, chunking_matches_sheet, format_probe, probe_engine
from paid_provider_proxy import system_fingerprint
from paid_test_support import PROFILES, load_fixture_sheet
from test_paid_preflight import build_fixture

SYSTEM = "Extract facts.\nThe conversation_date is: 2026-01-01\nReturn JSON."


class ProbeEngine:
    """Behaves like the real engine's probe surface: readiness status + one provider request per ingest."""
    instances = []

    def __init__(self, proxy_url, proxy_token, *, readiness=400, system=SYSTEM, fail_start=False):
        self.proxy_url, self.proxy_token = proxy_url, proxy_token
        self.readiness, self.system, self.fail_start = readiness, system, fail_start
        self.started = self.closed = False
        ProbeEngine.instances.append(self)

    def start(self, work, reuse=False):
        if self.fail_start:
            raise RuntimeError("engine refused to start")
        self.started = True

    def readiness_probe(self):
        return self.readiness

    def ingest_session(self, session):
        body = {"model": "gpt-6-luna", "messages": [{"role": "system", "content": self.system},
                                                    {"role": "user", "content": f"[CHUNK id=\"0\"]\n{session['turns'][0]['content']}\n[/CHUNK]"}],
                "max_completion_tokens": 16384, "reasoning_effort": "low"}
        request = Request(self.proxy_url + "/chat/completions", data=json.dumps(body).encode("utf-8"),
                          headers={"Content-Type": "application/json", "Authorization": "Bearer " + self.proxy_token})
        try:
            urlopen(request, timeout=5).close()
        except Exception as exc:  # the capture answers 400: that is the designed outcome
            raise RuntimeError("extraction failed") from exc

    def close(self):
        self.closed = True


def factory(**kwargs):
    return lambda url, token: ProbeEngine(url, token, **kwargs)


class ProbeEngineTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fixture = build_fixture(self.root)
        self.sheet = load_fixture_sheet(self.root)
        self.spec = self.fixture.spec(self.root / "c", mode="paid")
        ProbeEngine.instances.clear()

    def test_evaluation_build_prompt_size_and_pin(self):
        pinned = {**self.spec, "extraction_system_sha256": system_fingerprint(SYSTEM)}
        result = probe_engine(pinned, self.sheet, mode="paid", engine_factory=factory())
        self.assertTrue(result["passed"], result["problems"])
        observed = result["observed"]
        self.assertEqual(observed["readiness_bad_body_status"], 400)
        self.assertEqual(observed["system_sha256"], system_fingerprint(SYSTEM))
        self.assertLessEqual(observed["wire_overhead_bytes"], observed["sheet_system_extraction"])
        self.assertEqual(observed["extraction_requests"], 1)
        self.assertTrue(ProbeEngine.instances[0].closed)
        self.assertIn("fingerprint", format_probe(result))

    def test_non_evaluation_build_is_a_problem(self):
        result = probe_engine(self.spec, self.sheet, mode="dry_run", engine_factory=factory(readiness=404))
        self.assertFalse(result["passed"])
        self.assertTrue(any("not an evaluation build" in p for p in result["problems"]))
        # axum answers 422 to a body that fails deserialisation: still the evaluation route.
        self.assertTrue(probe_engine(self.spec, self.sheet, mode="dry_run", engine_factory=factory(readiness=422))["passed"])
        other = probe_engine(self.spec, self.sheet, mode="dry_run", engine_factory=factory(readiness=503))
        self.assertTrue(any("expected 400/422" in p for p in other["problems"]))

    def test_missing_pin_fails_only_in_paid_mode(self):
        self.assertTrue(probe_engine(self.spec, self.sheet, mode="dry_run", engine_factory=factory())["passed"])
        result = probe_engine(self.spec, self.sheet, mode="paid", engine_factory=factory())
        self.assertTrue(any("not pinned" in p for p in result["problems"]))
        wrong = {**self.spec, "extraction_system_sha256": "0" * 64}
        result = probe_engine(wrong, self.sheet, mode="paid", engine_factory=factory())
        self.assertTrue(any("differs from the pin" in p for p in result["problems"]))

    def test_oversized_system_prompt_is_a_problem(self):
        huge = SYSTEM + "x" * (int(self.sheet.module.SYSTEM["extraction"]) + 1)
        result = probe_engine(self.spec, self.sheet, mode="dry_run", engine_factory=factory(system=huge))
        self.assertTrue(any("exceeds the sheet" in p for p in result["problems"]))

    def test_failed_start_is_reported_and_closed(self):
        result = probe_engine(self.spec, self.sheet, mode="dry_run", engine_factory=factory(fail_start=True))
        self.assertFalse(result["passed"])
        self.assertTrue(any("engine probe failed" in p for p in result["problems"]))
        self.assertTrue(ProbeEngine.instances[0].closed)

    def test_chunking_matches_sheet(self):
        ok, detail = chunking_matches_sheet(self.sheet, self.spec["extraction"])
        self.assertTrue(ok, detail)
        bad = {**self.spec["extraction"], "input_chunk_tokens": 3000}
        self.assertFalse(chunking_matches_sheet(self.sheet, bad)[0])
        clamped = {**self.spec["extraction"], "max_transcript_tokens": 2000}
        self.assertFalse(chunking_matches_sheet(self.sheet, clamped)[0])

    def test_probe_session_is_synthetic(self):
        self.assertEqual(PROBE_SESSION["session_id"], "preflight-probe")
        self.assertEqual(PROFILES["extraction"]["max_completion_tokens"], 16384)
        self.assertIsNone(os.environ.get("OPENAI_API_KEY_NEVER_SET_BY_THIS_TEST"))


if __name__ == "__main__":
    unittest.main()
