import json
import os
from pathlib import Path
import tempfile
import unittest

from paid_prompts import STAGES, Prompts, PromptsError, StagePrompt, load_prompts
from paid_test_support import STAGE_SCHEMAS, STAGE_USER, plan_document, sha256_bytes, write_judge_prompt, write_plan

JUDGE_ORDER = ("source_scope", "source_conversations", "question", "reference", "candidate")
JUDGE_PARTS = {"candidate": "c", "question": "q", "reference": "r", "source_conversations": [], "source_scope": {}}


def calls_of(document, stage):
    return [c for c in document["calls"] if c["stage"] == stage]


class PromptsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path, self.sha = write_plan(self.root)
        self.prompts = load_prompts(self.path, self.sha)

    def load_document(self, document, sub="alt"):
        (self.root / sub).mkdir(exist_ok=True)
        return load_prompts(*write_plan(self.root / sub, document))

    def test_pinning(self):
        with self.assertRaises(PromptsError):
            load_prompts(self.path, "0" * 64)
        bad = self.root / "bad.json"
        bad.write_bytes(b"not json at all")
        with self.assertRaises(PromptsError):
            load_prompts(bad, sha256_bytes(bad.read_bytes()))
        with self.assertRaises(PromptsError):
            self.load_document({"schema_version": 1})

    def test_stages(self):
        self.assertEqual(STAGES, ("diagnostic", "audit", "selection", "answer", "judge"))
        for stage in STAGES:
            prompt = self.prompts.stage(stage)
            self.assertIsInstance(prompt, StagePrompt)
            self.assertEqual(prompt.user_keys, tuple(STAGE_USER[stage]))
            self.assertEqual(prompt.system, f"SYNTHETIC {stage} SYSTEM")
        self.assertIsNone(self.prompts.stage("answer").response_schema)
        self.assertEqual(self.prompts.stage("judge").response_schema["required"], ["status", "correct", "answer_span", "reason"])
        with self.assertRaises(PromptsError):
            self.prompts.stage("review")

    def test_inconsistent_plans_are_refused(self):
        def tweak(mutate, stage="audit"):
            document = plan_document()
            mutate(calls_of(document, stage)[1]["template"])
            return document

        def other_system(template):
            template["messages"][0]["content"] = "OTHER"

        def other_schema(template):
            template["response_schema"] = STAGE_SCHEMAS["selection"]

        def other_keys(template):
            template["messages"][1]["content"] = json.dumps({"facts": [], "question": "q"})

        four = plan_document()
        four["calls"] = [c for c in four["calls"] if c["stage"] != "judge"]
        for name, document in (("system", tweak(other_system)), ("schema", tweak(other_schema)),
                               ("keys", tweak(other_keys)), ("four stages", four)):
            with self.subTest(name=name), self.assertRaises(PromptsError):
                self.load_document(document, name.replace(" ", "_"))

    def test_render_user(self):
        self.assertEqual(self.prompts.render_user("diagnostic", {"facts": [], "question": "q"}), '{"facts":[],"question":"q"}')
        self.assertEqual(self.prompts.render_user("diagnostic", {"question": "ż", "facts": []}), '{"facts":[],"question":"ż"}')
        with self.assertRaises(PromptsError):
            self.prompts.render_user("diagnostic", {"facts": []})
        with self.assertRaises(PromptsError):
            self.prompts.render_user("diagnostic", {"facts": [], "question": "q", "extra": 1})

    def test_messages_and_response_format(self):
        messages = self.prompts.messages("diagnostic", {"facts": [], "question": "q"})
        self.assertEqual(messages, [{"role": "system", "content": "SYNTHETIC diagnostic SYSTEM"},
                                    {"role": "user", "content": '{"facts":[],"question":"q"}'}])
        self.assertEqual(self.prompts.response_format("judge"),
                         {"type": "json_schema", "json_schema": {"name": "judge", "strict": True, "schema": STAGE_SCHEMAS["judge"]}})
        self.assertIsNone(self.prompts.response_format("answer"))

    def test_fingerprint(self):
        fingerprint = self.prompts.fingerprint()
        self.assertEqual(fingerprint, load_prompts(self.path, self.sha).fingerprint())
        self.assertEqual(set(fingerprint), set(STAGES) | {"plan_json", "judge_prompt"})
        self.assertEqual((fingerprint["plan_json"], fingerprint["judge_prompt"]), (self.sha, None))
        changed = plan_document()
        for call in calls_of(changed, "diagnostic"):
            call["template"]["messages"][0]["content"] = "CHANGED diagnostic SYSTEM"
        other = self.load_document(changed).fingerprint()
        self.assertNotEqual(other["diagnostic"], fingerprint["diagnostic"])
        self.assertEqual({s: other[s] for s in STAGES if s != "diagnostic"},
                         {s: fingerprint[s] for s in STAGES if s != "diagnostic"})

    def test_judge_prompt_override(self):
        jp, jsha = write_judge_prompt(self.root, self.sha)
        prompts = load_prompts(self.path, self.sha, (jp, jsha))
        self.assertEqual(prompts.stage("judge").user_keys, JUDGE_ORDER)
        self.assertTrue(prompts.render_user("judge", JUDGE_PARTS).startswith('{"source_scope"'))
        self.assertEqual(prompts.fingerprint()["judge_prompt"], jsha)
        self.assertNotEqual(prompts.fingerprint()["judge"], self.prompts.fingerprint()["judge"])
        self.assertEqual(prompts.stage("judge").system, self.prompts.stage("judge").system)
        with self.assertRaises(PromptsError):
            load_prompts(self.path, self.sha, (jp, "0" * 64))

    def test_judge_prompt_disagreements_are_refused(self):
        variants = {
            "derived": dict(plan_sha256="1" * 64),
            "system": dict(system="OTHER judge SYSTEM"),
            "schema": dict(response_schema=STAGE_SCHEMAS["audit"]),
            "order": dict(order=["source_scope", "source_conversations", "question", "reference"]),
        }
        for name, kwargs in variants.items():
            sub = self.root / name
            sub.mkdir()
            kwargs.setdefault("plan_sha256", self.sha)
            with self.subTest(name=name), self.assertRaises(PromptsError):
                load_prompts(self.path, self.sha, write_judge_prompt(sub, **kwargs))


class RealPlanTests(unittest.TestCase):
    def test_real_plan_when_pinned(self):
        path, sha = os.environ.get("LOOMEM_PLAN_JSON"), os.environ.get("LOOMEM_PLAN_JSON_SHA256")
        if not path or not sha:
            self.skipTest("LOOMEM_PLAN_JSON / LOOMEM_PLAN_JSON_SHA256 not set")
        prompts = load_prompts(path, sha)
        self.assertIsInstance(prompts, Prompts)
        self.assertEqual(sorted(prompts.fingerprint()), sorted(set(STAGES) | {"plan_json", "judge_prompt"}))
        for stage in STAGES:
            self.assertTrue(prompts.stage(stage).system)
        self.assertEqual(sorted(prompts.stage("judge").user_keys), sorted(JUDGE_ORDER))


if __name__ == "__main__":
    unittest.main()
