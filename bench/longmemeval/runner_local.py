"""Owned local Loomem subprocess adapter; fresh database for each question.

Uses only synchronous raw stores plus the local embedding queue. It does not
claim readiness of extraction/consolidation (both disabled in this experiment).
"""
import json
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess
import time
from urllib.request import Request

from runner import Adapter
from validate import fail, sha256


DISABLED = ("scheduler", "knowledge_extraction", "entity_extraction", "memory_generator",
            "contradiction", "conversation_extraction", "profile", "dream", "content_type",
            "worker.backup", "search.cache", "search.tier1", "associator")


def replace_setting(text, section, key, value):
    pattern = r"(?m)^\[" + re.escape(section) + r"\]\s*\n(.*?)(?=^\[|\Z)"
    match = re.search(pattern, text, re.S | re.M)
    line = key + " = " + json.dumps(value)
    if not match:
        return text + "\n[" + section + "]\n" + line + "\n"
    body, count = re.subn(r"(?m)^" + re.escape(key) + r"\s*=.*$", line, match[1])
    if not count:
        body += line + "\n"
    return text[:match.start(1)] + body + text[match.end(1):]


def verify_artifacts(settings):
    config = settings["local_engine"]
    if set(config) != {"binary", "binary_sha256", "template", "template_sha256",
                       "model_dir", "model_onnx_sha256", "tokenizer_sha256"}:
        fail("Invalid local_engine configuration")
    artifacts = [(config["binary"], config["binary_sha256"]),
                 (config["template"], config["template_sha256"]),
                 (Path(config["model_dir"]) / "model.onnx", config["model_onnx_sha256"]),
                 (Path(config["model_dir"]) / "tokenizer.json", config["tokenizer_sha256"])]
    for path, expected in artifacts:
        if not Path(path).is_absolute() or sha256(path) != expected:
            fail("Local engine artifact hash mismatch")
    return config


def effective_config(settings, work):
    config = verify_artifacts(settings)
    text = Path(config["template"]).read_text()
    overrides = [("storage", "data_dir", str(work / "data")),
                 ("storage", "vector_enabled", True), ("storage.tantivy", "enabled", True),
                 ("storage.intent_log", "dir", "wal"), ("event_log", "dir", "events"),
                 ("content_type", "model", "disabled"),
                 ("server", "host", "127.0.0.1"),
                 ("server", "port", int(settings["endpoint"].rsplit(":", 1)[1])),
                 ("server", "auth_token_env", "LOOMEM_AUTH_TOKEN"),
                 ("llm", "api_key", ""), ("llm", "api_key_env", "BENCH_NO_API_KEY"),
                 ("llm", "embedding_provider", "local"), ("llm", "embedding_dim", 384),
                 ("llm", "embedding_model_path", config["model_dir"]),
                 ("search", "multi_query_enabled", False), ("search", "rerank_enabled", False),
                 ("search", "entities_file", str(work / "entities.toml")),
                 ("search", "synonyms_file", str(work / "synonyms.toml")),
                 ("pii", "blocklist_file", str(work / "pii_blocklist.txt"))]
    for section, key, value in overrides:
        text = replace_setting(text, section, key, value)
    for section in DISABLED:
        text = replace_setting(text, section, "enabled", False)
    return text


class LocalEngineAdapter(Adapter):
    def __init__(self, settings):
        super().__init__(settings)
        if not self.endpoint.startswith("http://127.0.0.1:"):
            fail("Owned engine requires 127.0.0.1")
        self.config = verify_artifacts(settings)
        self.root = Path(settings["data_dir"])
        self.process = None
        self.counter = 0
        self.mapping = {}
        # A new invocation always owns a fresh child directory. Never reuse or delete data.
        self.root.mkdir(parents=True, exist_ok=True)
        self.invocation = self.root / secrets.token_hex(16)
        self.invocation.mkdir(mode=0o700)
        (self.invocation / "identity.json").write_text(json.dumps({
            key: settings[key] for key in ("instance_id", "run_id", "data_dir")}))

    def engine_request(self, path, payload=None):
        body = None if payload is None else json.dumps(payload).encode()
        request = Request(self.endpoint + path, data=body, headers={
            "Content-Type": "application/json", "Authorization": "Bearer " + self.token})
        with self.opener.open(request, timeout=30) as response:
            if response.status != 200:
                fail("Engine requires HTTP 200")
            return json.load(response)

    def identity(self):
        self.close()
        self.counter += 1
        work = self.invocation / str(self.counter)
        work.mkdir(mode=0o700)
        (work / "config.toml").write_text(effective_config(self.settings, work))
        (work / "entities.toml").write_text("")
        (work / "synonyms.toml").write_text("synonyms = []\n")
        (work / "pii_blocklist.txt").write_text("")
        self.mapping = {}
        self.token = secrets.token_hex(32)
        # Fail before launching when another listener occupies the requested port.
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", int(self.endpoint.rsplit(":", 1)[1])))
        env = {"PATH": os.defpath, "HOME": str(work), "LOOMEM_CONFIG": str(work / "config.toml"),
               "LOOMEM_AUTH_TOKEN": self.token, "RUST_LOG": "error"}
        self.process = subprocess.Popen([self.config["binary"]], cwd=work, env=env,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                fail("Owned engine exited")
            try:
                status = self.engine_request("/v1/status")["config_summary"]
                if status["tantivy_docs"] != 0 or status["embeddings_count"] != 0:
                    fail("Owned database must start empty")
                return
            except OSError:
                time.sleep(0.05)
        fail("Owned engine startup timeout")

    def request(self, path, payload=None):
        if self.process is None or self.process.poll() is not None:
            fail("Owned engine unavailable")
        if path == "/ingest":
            return self.ingest(payload)
        if path == "/ready":
            return self.ready(payload)
        if path == "/search":
            found = self.engine_request("/v1/search", {key: payload[key] for key in
                                         ("query", "top_k", "trace")})
            ids = [self.mapping[hit["id"]] for hit in found["results"]]
            trace = found.get("trace_metadata", {})
            for stage in trace.get("evidence", {}).get("stages", []):
                stage["chunk_ids"] = [self.mapping[item] for item in stage["chunk_ids"]]
            return {"retrieved_ids": ids, "trace_metadata": trace}
        fail("Unsupported local adapter operation")

    def ingest(self, payload):
        accepted = []
        for record in payload["records"]:
            # Session labels and annotations never enter engine metadata. The complete
            # source record is structured untrusted data; source_id stays in local mapping.
            content = json.dumps({key: record[key] for key in ("role", "date", "content")})
            stored = self.engine_request("/v1/store", {"content": content, "persistent": True})
            if stored.get("status") != "stored" or not isinstance(stored.get("id"), str):
                fail("Store did not acknowledge persistence")
            if stored["id"] in self.mapping:
                fail("Duplicate engine ID")
            self.mapping[stored["id"]] = record["record_id"]
            accepted.append(record["record_id"])
        return {"status": "complete", "accepted_ids": accepted}

    def ready(self, payload):
        expected = len(payload["expected_ids"])
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            status = self.engine_request("/v1/status")["config_summary"]
            sync = self.engine_request("/v1/health/index-sync")
            if (status["embeddings_count"] == expected and status["tantivy_docs"] == expected
                    and sync["rocksdb_chunks"] == expected and sync["tantivy_docs"] == expected):
                return {"ready": True, "pending_jobs": 0, "indexes_ready": True,
                        "indexed_ids": list(self.mapping.values())}
            time.sleep(0.05)
        fail("Local raw-store indexing incomplete")

    def close(self):
        if self.process is not None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
            self.process = None
