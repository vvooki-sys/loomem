"""Real-engine smoke: FakeOpenAI upstream -> ProviderProxy -> sandboxed EvaluationEngine.

Skipped unless macOS ``sandbox-exec`` and the two env vars ``LOOMEM_EVAL_BINARY``
(an evaluation-feature ``loomem-server``) and ``LOOMEM_EVAL_MODEL_DIR`` are
present. Everything stays on 127.0.0.1: the synthetic 1-history / 2-session
holdout is ingested with extraction ON, every extraction request is reserved
through the proxy, readiness-v2 and the exact-bytes projection are proven,
``/v1/search`` fragment hits are checked against the stored source bytes, and
the sandbox profile is probed for an immediate EPERM on a non-loopback connect.
"""
import errno
import json
import os
from pathlib import Path
import platform
import re
import socket
import subprocess
import sys
import tempfile
import time
import unittest

from paid_call_journal import CallJournal
from paid_engine import EvaluationEngine, predicted_chunks, render_transcript
from paid_fake_openai import FakeOpenAI
from paid_openai_wire import Transport
from paid_price_sheet import load_sheet
from paid_provider_proxy import ProviderProxy
from paid_test_support import PROFILES, SHEET_SOURCE, history_doc, make_campaign, write_sheet
from smoke_local_e1 import PROFILE, SANDBOX
from validate import sha256

BINARY, MODEL_DIR = os.environ.get("LOOMEM_EVAL_BINARY"), os.environ.get("LOOMEM_EVAL_MODEL_DIR")
TEMPLATE = Path(__file__).resolve().parents[2] / "config.toml"
AVAILABLE = bool(platform.system() == "Darwin" and Path(SANDBOX).is_file() and BINARY and MODEL_DIR)
EXTRACTION = {"max_completion_tokens": 16384, "input_chunk_tokens": 4000, "split_on_truncation": True,
              "max_facts_per_chunk": 20, "completion_tokens_floor": 0, "max_transcript_tokens": 20000}
# The engine's real extraction system prompt is ~6.1 KB once JSON-encoded; the
# fixture sheet's SYSTEM["extraction"] = 6000 is too small for the P1 check
# (wire bytes + FRAMING <= R_i), so the smoke sheet widens that one entry.
SMOKE_SHEET = SHEET_SOURCE.replace('"extraction": 6_000', '"extraction": 8_000')
FRAGMENT_ID = re.compile(r"^[0-9a-f-]{36}:f(?:0|[1-9][0-9]*)$")
PROBE = ("import socket, sys\n"
         "sock = socket.socket()\n"
         "sock.settimeout(5)\n"
         "try:\n    sock.connect((sys.argv[1], int(sys.argv[2])))\n"
         "except OSError as error:\n    print('errno', error.errno)\n"
         "else:\n    print('connected')\n"
         "finally:\n    sock.close()\n")


def artifacts():
    model = Path(MODEL_DIR)
    return {"binary": str(Path(BINARY).resolve()), "binary_sha256": sha256(BINARY),
            "template": str(TEMPLATE), "template_sha256": sha256(TEMPLATE),
            "model_dir": str(model), "model_onnx_sha256": sha256(model / "model.onnx"),
            "tokenizer_sha256": sha256(model / "tokenizer.json")}


def sandboxed_connect(host, port):
    started = time.monotonic()
    run = subprocess.run([SANDBOX, "-p", PROFILE, sys.executable, "-I", "-c", PROBE, host, str(port)],
                         capture_output=True, text=True, timeout=60, check=False)
    return run.stdout.strip(), time.monotonic() - started, run.stderr[-300:]


@unittest.skipUnless(AVAILABLE, "needs macOS sandbox-exec, LOOMEM_EVAL_BINARY and LOOMEM_EVAL_MODEL_DIR")
class SandboxSmokeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="loomem-paid-smoke-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        previous = os.environ.get("PAID_TEST_KEY")
        os.environ["PAID_TEST_KEY"] = "synthetic"
        self.addCleanup(lambda: os.environ.pop("PAID_TEST_KEY") if previous is None
                        else os.environ.__setitem__("PAID_TEST_KEY", previous))
        path, digest = write_sheet(self.root, SMOKE_SHEET)
        self.sheet = load_sheet(path, digest)
        self.campaign = make_campaign(self.root, self.sheet)
        self.journal = CallJournal(self.campaign.directory)

    def test_two_sessions_extract_prove_fetch_search_and_sandbox_eperm(self):
        summary = {"sessions": {}}
        history = history_doc("h1")
        with FakeOpenAI({"extraction": "ok"}, model_by_role={"extraction": "gpt-6-luna"}) as fake:
            transport = Transport(fake.url, "PAID_TEST_KEY", {"127.0.0.1"})
            with ProviderProxy(self.campaign, self.journal, self.sheet, transport,
                               extraction_profile=PROFILES["extraction"], manifest_sha256="a" * 64) as proxy:
                engine = EvaluationEngine(artifacts(), proxy_url=proxy.url, proxy_token=proxy.token,
                                          extraction=EXTRACTION, top_k=60)
                started = time.monotonic()
                engine.start(self.root / "engine" / "h1")
                summary["engine_start_s"] = round(time.monotonic() - started, 2)
                try:
                    self.exercise(engine, proxy, fake, history, summary)
                finally:
                    engine.close()
            summary["fake_roles"] = [entry["role"] for entry in fake.log]
            self.assertEqual(summary["fake_roles"], ["extraction", "extraction"], "zero NER / other requests")
        self.probe_sandbox(summary)
        print("\nSANDBOX SMOKE " + json.dumps(summary, indent=1, sort_keys=True))

    def exercise(self, engine, proxy, fake, history, summary):
        receipts = {}
        for session in history["sessions"]:
            sid = session["session_id"]
            content = render_transcript(session)
            chunks = predicted_chunks(content, self.sheet.extraction_chunk_chars())
            self.assertEqual(chunks, 1, "short synthetic sessions are one chunk")
            proxy.bind("h1", sid, chunks * 5)
            receipt = engine.ingest_session(session)
            records = proxy.unbind()
            self.assertEqual([r["outcome"] for r in records], ["ok"], f"exactly one extraction request for {sid}")
            self.assertFalse(receipt.zero_facts)
            self.assertEqual(receipt.content, content)
            self.assertEqual(len(receipt.fragment_ids), 2)
            proof = engine.prove_readiness_v2(f"smoke-{sid}", receipt, session, 90)
            self.assertTrue(proof["response"]["ready"])
            self.assertEqual(engine.fetch_source(receipt.source_id)["content"], content)
            receipts[sid] = receipt
            user_bytes = len(fake.log[-1]["wire"]["messages"][1]["content"].encode())
            summary["sessions"][sid] = {
                "extraction_requests": len(records), "ordinals": [r["ordinal"] for r in records],
                "facts": len(receipt.fact_ids), "fragments": len(receipt.fragment_ids),
                "readiness_records": len(proof["response"]["records"]), "ready": proof["response"]["ready"],
                "wire_bytes": records[0]["wire_bytes"], "user_content_bytes": user_bytes,
                "usage": records[0]["usage"], "mcp_text": receipt.raw["content"][0]["text"]}
        self.assertFalse(proxy.halted, proxy.halt_reason)
        self.assertEqual(len(proxy.records), 2)
        self.assertEqual({r["system_sha256"] for r in proxy.records}, {proxy.system_sha256})
        self.assertEqual(self.campaign.ledger.totals()["request_count"], 2)
        self.assertEqual(sorted(s["outcome"] for s in self.journal.state.settled.values()), ["ok", "ok"])
        summary["system_sha256"] = proxy.system_sha256
        summary["pins"] = {alias: pin["model"] for alias, pin in self.journal.state.pins.items()}
        found = engine.search("What was synthetic turn 0 of h1?", "2026-04-02")
        hits = found["results"]
        fragments = [hit for hit in hits if FRAGMENT_ID.fullmatch(hit["id"])]
        self.assertTrue(fragments, "search must return fragment hits")
        for hit in fragments:
            evidence = hit["evidence"]
            source = engine.sources[evidence["source_id"]]
            self.assertEqual(hit["content"], source.encode()[evidence["byte_start"]:evidence["byte_end"]].decode())
            self.assertEqual(hit["metadata"]["level"], 0)
            self.assertIn(evidence["author"], ("user", "assistant"))
            self.assertIn(evidence["conversation_date"], ("2025-07-10", "2025-07-11"))
        facts = [hit for hit in hits if hit["metadata"]["level"] == 1]
        self.assertTrue(set(hit["id"] for hit in facts) <= {f for r in receipts.values() for f in r.fact_ids})
        summary["search"] = {"hits": len(hits), "fragment_hits": len(fragments), "fact_hits": len(facts),
                             "reference_time": "2026-04-02T00:00:00Z",
                             "evidence_keys": sorted(fragments[0]["evidence"])}
        summary["effective_config_sha256"] = engine.effective_config_sha256

    def probe_sandbox(self, summary):
        outside, elapsed, stderr = sandboxed_connect("10.255.255.1", 9)
        self.assertEqual(outside, f"errno {errno.EPERM}", stderr)
        self.assertLess(elapsed, 5.0, "the deny must be immediate, not a timeout")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            loopback = sandboxed_connect("127.0.0.1", listener.getsockname()[1])[0]
        self.assertEqual(loopback, "connected")
        summary["sandbox_probe"] = {"outside": outside, "elapsed_s": round(elapsed, 2), "loopback": loopback}


if __name__ == "__main__":
    unittest.main()
