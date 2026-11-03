//! Revocation must never tear a dream subject apart: an apply section in
//! progress finishes (merged chunk, retired sources, embedding request) and
//! the job stops before the next subject; a job revoked before any section
//! leaves nothing behind. Drives the real `hosted_shared_engine` binary over
//! stdio with a loopback LLM fixture.
#![cfg(all(feature = "hosted-engine", feature = "evaluation"))]

use axum::{routing::post, Json, Router};
use regex::Regex;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REPLY_TIMEOUT: Duration = Duration::from_secs(120);
const APPLY_HOLD_MARKER: &str = "LOOMEM_HOSTED_HOLD_APPLY";
const REVOKE_DEFERRED_MARKER: &str = "LOOMEM_HOSTED_REVOKE_DEFERRED";

/// Loopback chat-completions endpoint that merges every subject and flags
/// each source it was shown as contradicted.
struct MockLlm {
    url: String,
    requests: Arc<Mutex<usize>>,
}

fn start_mock_llm() -> MockLlm {
    let requests = Arc::new(Mutex::new(0));
    let seen = requests.clone();
    let (url_tx, url_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("mock runtime");
        runtime.block_on(async move {
            let app = Router::new().route(
                "/v1/chat/completions",
                post(move |Json(body): Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        *seen.lock().expect("request counter") += 1;
                        Json(dream_reply(&body))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback");
            let port = listener.local_addr().expect("local addr").port();
            url_tx
                .send(format!("http://127.0.0.1:{port}/v1/chat/completions"))
                .expect("deliver url");
            axum::serve(listener, app).await.expect("serve mock");
        });
    });
    let url = url_rx.recv().expect("mock url");
    MockLlm { url, requests }
}

fn dream_reply(request: &Value) -> Value {
    let prompt = request["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    let ids = Regex::new(r"Source id: (\S+)").expect("regex");
    let contradictions: Vec<Value> = ids
        .captures_iter(prompt)
        .map(|capture| json!({"old_uuid": capture[1].to_string(), "reason": "merged"}))
        .collect();
    let content = json!({
        "merged_fact": "synthetic merged fact: the subject is consolidated",
        "fact_type": "fact",
        "fact_date": null,
        "contradictions": contradictions,
        "confidence": 0.9,
    });
    json!({
        "choices": [{"finish_reason": "stop", "message": {"content": content.to_string()}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10},
    })
}

fn write_config(dir: &Path) -> PathBuf {
    let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.toml");
    let mut text = std::fs::read_to_string(reference).expect("read reference config");
    let entities = dir.join("entities.toml");
    std::fs::write(&entities, "").expect("write entities");
    replace_once(
        &mut text,
        "embedding_provider = \"local\"",
        "embedding_provider = \"openai\"",
    );
    replace_once(&mut text, "vector_enabled = true", "vector_enabled = false");
    replace_once(
        &mut text,
        "synonyms_file = \"synonyms.toml\"",
        &format!(
            "synonyms_file = \"{}\"",
            dir.join("synonyms.toml").display()
        ),
    );
    replace_once(
        &mut text,
        "entities_file = \"entities.toml\"",
        &format!("entities_file = \"{}\"", entities.display()),
    );
    for section in [
        "[scheduler]",
        "[entity_extraction]",
        "[knowledge_extraction]",
    ] {
        disable_section(&mut text, section);
    }
    let path = dir.join("config.toml");
    std::fs::write(&path, text).expect("write config");
    path
}

/// Replaces one setting line; comments mentioning the same text do not count.
fn replace_once(text: &mut String, old: &str, new: &str) {
    let (old, new) = (format!("\n{old}"), format!("\n{new}"));
    assert_eq!(text.matches(&old).count(), 1, "{old} must appear once");
    *text = text.replace(&old, &new);
}

fn disable_section(text: &mut String, header: &str) {
    let needle = format!("\n{header}\n");
    let start = text
        .find(&needle)
        .unwrap_or_else(|| panic!("{header} section"));
    let enabled = "\nenabled = true";
    let offset = text[start..].find(enabled).expect("enabled line");
    let at = start + offset + 1;
    text.replace_range(at..at + "enabled = true".len(), "enabled = false");
}

struct Engine {
    child: Child,
    stdin: ChildStdin,
    stdout: Receiver<Value>,
    stderr: Receiver<String>,
    next_id: u64,
}

impl Engine {
    fn start(root: &Path, config: &Path, llm_url: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hosted_shared_engine"))
            .arg(root)
            .arg(config)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("OPENAI_API_KEY", "synthetic-test-key")
            .env("LOOMEM_EVALUATION_DREAM_URL", llm_url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hosted_shared_engine");
        let stdin = child.stdin.take().expect("stdin");
        let (stdout_tx, stdout) = mpsc::channel();
        let out = BufReader::new(child.stdout.take().expect("stdout"));
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                let reply: Value = serde_json::from_str(&line).expect("reply is JSON");
                if stdout_tx.send(reply).is_err() {
                    break;
                }
            }
        });
        let (stderr_tx, stderr) = mpsc::channel();
        let err = BufReader::new(child.stderr.take().expect("stderr"));
        std::thread::spawn(move || {
            for line in err.lines().map_while(Result::ok) {
                eprintln!("[engine] {line}");
                if stderr_tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            stdout,
            stderr,
            next_id: 0,
        }
    }

    fn send(&mut self, mut request: Value) -> String {
        self.next_id += 1;
        let id = self.next_id.to_string();
        request["request_id"] = Value::String(id.clone());
        writeln!(self.stdin, "{request}").expect("write request");
        self.stdin.flush().expect("flush request");
        id
    }

    fn reply(&mut self) -> Value {
        match self.stdout.recv_timeout(REPLY_TIMEOUT) {
            Ok(reply) => reply,
            Err(RecvTimeoutError::Timeout) => panic!("no reply within {REPLY_TIMEOUT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("engine closed stdout"),
        }
    }

    /// Sends one request and returns its reply; nothing else may be in flight.
    fn request(&mut self, request: Value) -> Value {
        let id = self.send(request);
        let reply = self.reply();
        assert_eq!(reply["request_id"], Value::String(id), "{reply}");
        reply
    }

    fn ok(&mut self, request: Value) -> Value {
        let reply = self.request(request);
        assert_eq!(reply["ok"], Value::Bool(true), "{reply}");
        reply
    }

    fn tool(&mut self, context: &Value, name: &str, args: Value) -> String {
        let op = format!("op-{}", self.next_id + 1);
        self.ok(json!({
            "cmd": "tool_async", "context": context, "op_id": op, "name": name, "args": args,
        }));
        let reply = self.ok(json!({"cmd": "await", "context": context, "op_id": op}));
        assert_ne!(reply["result"]["isError"], Value::Bool(true), "{reply}");
        reply["result"]["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    fn store(&mut self, context: &Value, subject: &str, content: &str) -> String {
        let text = self.tool(
            context,
            "memory_store",
            json!({"content": content, "subject": subject}),
        );
        Regex::new(r"\(id: ([^\s)]+)")
            .expect("regex")
            .captures(&text)
            .map(|capture| capture[1].to_string())
            .unwrap_or_else(|| panic!("stored id in {text}"))
    }

    fn wait_for_stderr(&self, marker: &str) {
        self.wait_for_stderr_count(marker, 1);
    }

    /// Waits until `marker` appeared `count` more times on stderr.
    fn wait_for_stderr_count(&self, marker: &str, count: usize) {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut seen = 0;
        while seen < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.stderr.recv_timeout(remaining) {
                Ok(line) if line.contains(marker) => seen += 1,
                Ok(_) => continue,
                Err(error) => panic!("{marker} seen {seen}/{count} on stderr: {error}"),
            }
        }
    }

    fn superseded(&mut self, context: &Value, source: &str) -> bool {
        self.tool(context, "memory_history", json!({"chunk_id": source}))
            .contains("[superseded]")
    }

    fn shutdown(mut self) {
        let reply = self.request(json!({"cmd": "shutdown"}));
        assert_eq!(reply["ok"], Value::Bool(true), "{reply}");
        drop(self.stdin);
        let status = self.child.wait().expect("engine exits");
        assert!(status.success(), "engine exit status {status}");
    }
}

fn context(actor: &str, scope: &str, epoch: i64) -> Value {
    json!({"actor": actor, "scope": scope, "epoch": epoch})
}

#[test]
fn revoke_waits_for_an_apply_section_and_a_revoked_job_leaves_nothing_half_done() {
    let llm = start_mock_llm();
    let temp = tempfile::tempdir().expect("tempdir");
    let config = write_config(temp.path());
    let mut engine = Engine::start(&temp.path().join("engine"), &config, &llm.url);

    // Scope A has two subjects and is revoked while the first apply section
    // is open: that section finishes whole, the second subject never starts.
    let a = context("actor-a", "scope-a", 1);
    engine.ok(json!({"cmd": "open_scope", "context": a, "generation": "live"}));
    let subjects: Vec<Vec<String>> = ["subject-one", "subject-two"]
        .iter()
        .map(|subject| {
            (1..=2)
                .map(|n| engine.store(&a, subject, &format!("synthetic {subject} source {n}")))
                .collect()
        })
        .collect();
    engine.ok(json!({
        "cmd": "tool_async", "context": a, "op_id": "dream", "name": "memory_dream",
        "args": {}, "failpoint": "hosted_hold_apply",
    }));
    engine.wait_for_stderr(APPLY_HOLD_MARKER);
    assert_eq!(*llm.requests.lock().expect("counter"), 1);
    let revoked = context("actor-a", "scope-a", 2);
    let revoke_id = engine.send(json!({"cmd": "revoke", "context": revoked}));
    // Revocation found the section open and is now waiting for it.
    engine.wait_for_stderr(REVOKE_DEFERRED_MARKER);
    let release_id = engine.send(json!({"cmd": "release_holds"}));
    let mut replies = std::collections::HashMap::new();
    for _ in 0..2 {
        let reply = engine.reply();
        let id = reply["request_id"].as_str().expect("request_id").to_owned();
        replies.insert(id, reply);
    }
    assert_eq!(replies[&release_id]["ok"], Value::Bool(true), "{replies:?}");
    assert_eq!(replies[&revoke_id]["ok"], Value::Bool(true), "{replies:?}");
    let late = engine.request(json!({"cmd": "await", "context": a, "op_id": "dream"}));
    assert_eq!(late["ok"], Value::Bool(false), "{late}");
    assert!(
        late["error"]
            .as_str()
            .is_some_and(|error| error.contains("stale scope epoch")),
        "{late}"
    );

    // Exactly one subject was applied whole; the other is untouched and no
    // second merge call was made before the job stopped.
    engine.ok(json!({"cmd": "open_scope", "context": revoked, "generation": "live"}));
    let applied: Vec<bool> = subjects
        .iter()
        .map(|sources| {
            let flags: Vec<bool> = sources
                .iter()
                .map(|source| engine.superseded(&revoked, source))
                .collect();
            assert!(
                flags.iter().all(|flag| *flag == flags[0]),
                "a subject must be retired whole or not at all: {flags:?}"
            );
            flags[0]
        })
        .collect();
    assert_eq!(
        applied.iter().filter(|flag| **flag).count(),
        1,
        "exactly one subject applied: {applied:?}"
    );
    assert_eq!(*llm.requests.lock().expect("counter"), 1, "no second merge");
    let remaining = engine.tool(&revoked, "memory_dream", json!({}));
    assert!(remaining.contains("Facts merged: 1"), "{remaining}");
    assert_eq!(*llm.requests.lock().expect("counter"), 2);

    // Scope B: revoked before any section started leaves nothing behind.
    let b = context("actor-b", "scope-b", 1);
    engine.ok(json!({"cmd": "open_scope", "context": b, "generation": "live"}));
    let sources: Vec<String> = (1..=2)
        .map(|n| {
            engine.store(
                &b,
                "subject-b",
                &format!("synthetic dream source {n} for scope b"),
            )
        })
        .collect();
    engine.ok(json!({
        "cmd": "tool_async", "context": b, "op_id": "dream", "name": "memory_dream",
        "args": {}, "failpoint": "hosted_hold",
    }));
    let revoked_b = context("actor-b", "scope-b", 2);
    engine.ok(json!({"cmd": "revoke", "context": revoked_b}));
    engine.ok(json!({"cmd": "open_scope", "context": revoked_b, "generation": "live"}));
    for source in &sources {
        assert!(!engine.superseded(&revoked_b, source), "{source}");
    }
    let merged = engine.tool(&revoked_b, "memory_dream", json!({}));
    assert!(merged.contains("Facts merged: 1"), "{merged}");
    assert_eq!(*llm.requests.lock().expect("counter"), 3);
    engine.shutdown();
}

#[test]
fn revoke_signals_every_job_before_waiting_for_any_section() {
    let llm = start_mock_llm();
    let temp = tempfile::tempdir().expect("tempdir");
    let config = write_config(temp.path());
    let mut engine = Engine::start(&temp.path().join("engine"), &config, &llm.url);
    let a = context("actor-a", "scope-a", 1);
    engine.ok(json!({"cmd": "open_scope", "context": a, "generation": "live"}));
    let subjects: Vec<Vec<String>> = ["subject-one", "subject-two", "subject-three"]
        .iter()
        .map(|subject| {
            (1..=2)
                .map(|n| engine.store(&a, subject, &format!("synthetic {subject} source {n}")))
                .collect()
        })
        .collect();
    // Two dream jobs of one scope, each held inside its first apply section.
    for op in ["dream-1", "dream-2"] {
        engine.ok(json!({
            "cmd": "tool_async", "context": a, "op_id": op, "name": "memory_dream",
            "args": {}, "failpoint": "hosted_hold_apply",
        }));
    }
    engine.wait_for_stderr_count(APPLY_HOLD_MARKER, 2);
    assert_eq!(*llm.requests.lock().expect("counter"), 2);
    let revoked = context("actor-a", "scope-a", 2);
    let revoke_id = engine.send(json!({"cmd": "revoke", "context": revoked}));
    // Both jobs are told to stop before revocation waits for either section;
    // joining the first job before signalling the second would stall here.
    engine.wait_for_stderr_count(REVOKE_DEFERRED_MARKER, 2);
    let release_id = engine.send(json!({"cmd": "release_holds"}));
    let mut replies = std::collections::HashMap::new();
    for _ in 0..2 {
        let reply = engine.reply();
        let id = reply["request_id"].as_str().expect("request_id").to_owned();
        replies.insert(id, reply);
    }
    assert_eq!(replies[&release_id]["ok"], Value::Bool(true), "{replies:?}");
    assert_eq!(replies[&revoke_id]["ok"], Value::Bool(true), "{replies:?}");
    // Each job finished only its open section and started no further subject.
    assert_eq!(*llm.requests.lock().expect("counter"), 2, "no third merge");
    engine.ok(json!({"cmd": "open_scope", "context": revoked, "generation": "live"}));
    let untouched = subjects
        .iter()
        .filter(|sources| {
            sources
                .iter()
                .all(|source| !engine.superseded(&revoked, source))
        })
        .count();
    assert!(untouched >= 1, "at least one subject stays untouched");
    engine.shutdown();
}
