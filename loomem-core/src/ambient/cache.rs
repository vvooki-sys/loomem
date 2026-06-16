//! In-memory ambient response cache + token-budget helpers.
//!
//! AC-9 (cache):
//! - Key: blake3 over `(user_id, scope, recent_turns_hash, hint)`.
//! - TTL: 60s default, override via env `LOOMEM_AMBIENT_CACHE_TTL_SECS`.
//! - Capacity: 10k entries, LRU eviction (last-access timestamp).
//! - Distributed cache deferred per `/103a-full`.
//!
//! AC-10 (staleness): TTL-only invalidation per Anna finding-3 decision.
//! Event-bus integration deferred to `/103a-full`. `refresh: true` on the
//! request bypasses cache for that call.
//!
//! AC-3 (token budget):
//! - Hard cap per-injection: 1500 tokens.
//! - Per-snippet cap: 200 tokens.
//! - Marker floor: 50 tokens (load-bearing semantics — never dropped for
//!   budget; compression strategy drops low-tier positive snippets first).
//! - Tokenizer: `tiktoken-rs` cl100k_base, lazily initialized once per process.

use std::collections::HashMap;
use std::env;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tiktoken_rs::{cl100k_base, CoreBPE};

use super::types::{AmbientResponse, AmbientSnippet, MarkerIfEmpty, RecentTurn, Tier};

/// AC-3 token-budget constants.
pub const TOKEN_BUDGET_HARD_CAP: usize = 1500;
pub const TOKEN_PER_SNIPPET_CAP: usize = 200;
pub const TOKEN_MARKER_FLOOR: usize = 50;

/// AC-9 cache constants.
pub const CACHE_DEFAULT_TTL_SECS: u64 = 60;
pub const CACHE_MAX_ENTRIES: usize = 10_000;
const CACHE_TTL_ENV: &str = "LOOMEM_AMBIENT_CACHE_TTL_SECS";

/// Resolved cache TTL — env override or default.
#[must_use]
pub fn cache_ttl() -> Duration {
    env::var(CACHE_TTL_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(
            Duration::from_secs(CACHE_DEFAULT_TTL_SECS),
            Duration::from_secs,
        )
}

/// Stable 32-byte cache key over the request fingerprint.
///
/// Hash inputs (in order, with explicit length-prefixed delimiters to avoid
/// concatenation collisions): `user_id`, `scope`, each `recent_turn` role +
/// content, optional `hint`, optional `stream` (cycle/103c — separate
/// retrieval-scope dimension; same `(user, scope, turns, hint)` triple with
/// different `stream` MUST hash to different keys to avoid serving wrong-
/// stream cached snippets).
#[must_use]
pub fn build_key(
    user_id: &str,
    scope: &str,
    recent_turns: Option<&[RecentTurn]>,
    hint: Option<&str>,
    stream: Option<&str>,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    write_lp(&mut h, user_id.as_bytes());
    write_lp(&mut h, scope.as_bytes());
    let turns = recent_turns.unwrap_or(&[]);
    write_lp(&mut h, &(turns.len() as u32).to_le_bytes());
    for t in turns {
        write_lp(&mut h, t.role.as_bytes());
        write_lp(&mut h, t.content.as_bytes());
    }
    write_lp(&mut h, hint.unwrap_or("").as_bytes());
    write_lp(&mut h, stream.unwrap_or("").as_bytes());
    *h.finalize().as_bytes()
}

/// Length-prefix a payload into the hasher to make the encoding injective —
/// `("ab", "c")` and `("a", "bc")` must hash to different keys.
fn write_lp(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// In-memory LRU cache for `AmbientResponse` keyed by `[u8; 32]` blake3 hash.
///
/// Eviction policy: TTL-first (any expired entry on `get` returns `None` and
/// is removed). When the cache exceeds `CACHE_MAX_ENTRIES`, the entry with
/// the oldest `last_access` is evicted. Bounded O(n) eviction is acceptable
/// at n=10k.
pub struct AmbientCache {
    inner: Mutex<HashMap<[u8; 32], CacheEntry>>,
    ttl: Duration,
    max_entries: usize,
    /// Monotonic access counter — drives LRU ordering.
    access_counter: Mutex<u64>,
}

struct CacheEntry {
    value: AmbientResponse,
    inserted_at: Instant,
    last_access: u64,
}

impl AmbientCache {
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(cache_ttl(), CACHE_MAX_ENTRIES)
    }

    #[must_use]
    pub fn with_config(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            max_entries,
            access_counter: Mutex::new(0),
        }
    }

    /// Fetch a non-expired entry. Removes expired entries opportunistically.
    pub fn get(&self, key: &[u8; 32]) -> Option<AmbientResponse> {
        let now = Instant::now();
        let next_access = self.bump_access();
        let mut map = self.inner.lock().ok()?;
        let expired = map
            .get(key)
            .is_some_and(|e| now.duration_since(e.inserted_at) >= self.ttl);
        if expired {
            map.remove(key);
            return None;
        }
        let entry = map.get_mut(key)?;
        entry.last_access = next_access;
        Some(entry.value.clone())
    }

    /// Insert a fresh entry; evict the oldest by `last_access` if at capacity.
    pub fn put(&self, key: [u8; 32], value: AmbientResponse) {
        let next_access = self.bump_access();
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
        if map.len() >= self.max_entries && !map.contains_key(&key) {
            evict_oldest(&mut map);
        }
        map.insert(
            key,
            CacheEntry {
                value,
                inserted_at: Instant::now(),
                last_access: next_access,
            },
        );
    }

    /// Test/admin helper — current entry count (post-expiry-on-get is
    /// best-effort; this returns physical size including stale entries).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn bump_access(&self) -> u64 {
        let mut c = self.access_counter.lock().expect("access_counter poisoned");
        *c += 1;
        *c
    }
}

impl Default for AmbientCache {
    fn default() -> Self {
        Self::new()
    }
}

fn evict_oldest(map: &mut HashMap<[u8; 32], CacheEntry>) {
    let oldest = map
        .iter()
        .min_by_key(|(_, e)| e.last_access)
        .map(|(k, _)| *k);
    if let Some(k) = oldest {
        map.remove(&k);
    }
}

/// Lazy global cl100k_base BPE encoder for token counting.
fn bpe() -> Result<&'static CoreBPE> {
    static BPE: OnceLock<CoreBPE> = OnceLock::new();
    if let Some(b) = BPE.get() {
        return Ok(b);
    }
    let b = cl100k_base().context("cl100k_base initialization failed")?;
    Ok(BPE.get_or_init(|| b))
}

/// Count tokens with cl100k_base (OpenAI tiktoken).
pub fn count_tokens(text: &str) -> Result<usize> {
    Ok(bpe()?.encode_with_special_tokens(text).len())
}

/// AC-3 compression strategy: drop low-tier positive snippets until the
/// `(snippets, marker)` payload fits the 1500-token hard cap. The marker
/// (when present) is preserved at all costs — its 50-token floor is
/// load-bearing semantics per `/103gate` §8.6 + probe-4 evidence.
///
/// Returns the (possibly trimmed) snippets vector. Marker passes through
/// the caller untouched. If even after dropping every positive snippet the
/// marker alone exceeds budget, the caller must escalate (stop-and-ask
/// trigger §Stop-and-ask "Token budget hard cap conflict z marker floor").
pub fn truncate_to_budget(
    snippets: Vec<AmbientSnippet>,
    marker: Option<&MarkerIfEmpty>,
) -> Result<Vec<AmbientSnippet>> {
    let marker_tokens = match marker {
        Some(m) => count_tokens(&serde_json::to_string(m)?)?,
        None => 0,
    };
    let mut budget = TOKEN_BUDGET_HARD_CAP.saturating_sub(marker_tokens);
    let kept = collect_within_budget(snippets, &mut budget)?;
    Ok(kept)
}

/// Drop low-tier first, then medium, then high — preserving the most
/// load-bearing snippets when budget pressure forces a cut.
fn collect_within_budget(
    mut snippets: Vec<AmbientSnippet>,
    budget: &mut usize,
) -> Result<Vec<AmbientSnippet>> {
    snippets.sort_by_key(|s| tier_drop_priority(s.tier));
    let mut kept: Vec<AmbientSnippet> = Vec::with_capacity(snippets.len());
    for s in snippets.into_iter().rev() {
        let n = count_tokens(&s.text)?;
        if n > TOKEN_PER_SNIPPET_CAP {
            continue;
        }
        if n <= *budget {
            *budget -= n;
            kept.push(s);
        }
    }
    kept.sort_by(|a, b| {
        tier_drop_priority(b.tier)
            .cmp(&tier_drop_priority(a.tier))
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    Ok(kept)
}

/// Lower number = drop FIRST under budget pressure. High/conflict are most
/// load-bearing; low is the first to go.
fn tier_drop_priority(tier: Tier) -> u8 {
    match tier {
        Tier::Low => 0,
        Tier::Medium => 1,
        Tier::High => 2,
        Tier::Conflict => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambient::types::{AmbientDebug, NegativeAmbientStatus, NegativeReason};

    fn snip(text: &str, tier: Tier, score: f32) -> AmbientSnippet {
        AmbientSnippet {
            text: text.to_string(),
            tier,
            score,
        }
    }

    fn empty_resp() -> AmbientResponse {
        AmbientResponse {
            snippets: vec![],
            marker_if_empty: None,
            debug: AmbientDebug::default(),
        }
    }

    #[test]
    fn build_key_is_deterministic_for_identical_inputs() {
        let k1 = build_key("u1", "private:u1", None, Some("hint"), None);
        let k2 = build_key("u1", "private:u1", None, Some("hint"), None);
        assert_eq!(k1, k2);
    }

    #[test]
    fn build_key_differs_when_user_id_differs() {
        let k1 = build_key("u1", "private:u1", None, None, None);
        let k2 = build_key("u2", "private:u2", None, None, None);
        assert_ne!(k1, k2);
    }

    #[test]
    fn build_key_resists_concat_collision() {
        // Without length-prefixing, ("ab", "c") would collide with ("a", "bc").
        let k1 = build_key("ab", "c", None, None, None);
        let k2 = build_key("a", "bc", None, None, None);
        assert_ne!(k1, k2);
    }

    #[test]
    fn build_key_includes_recent_turns() {
        let turns = vec![RecentTurn {
            role: "user".to_string(),
            content: "hello".to_string(),
        }];
        let k_with = build_key("u", "s", Some(&turns), None, None);
        let k_without = build_key("u", "s", None, None, None);
        assert_ne!(k_with, k_without);
    }

    #[test]
    fn build_key_includes_stream_for_103c_isolation() {
        // /103c cycle: same (user, scope, turns, hint) with different `stream`
        // MUST hash to different keys — otherwise serving cached snippets from
        // a different stream's retrieval would be a privacy/correctness bug.
        let k_a = build_key("u", "s", None, None, Some("lme_q1"));
        let k_b = build_key("u", "s", None, None, Some("lme_q2"));
        let k_none = build_key("u", "s", None, None, None);
        assert_ne!(k_a, k_b, "stream A vs stream B → different keys");
        assert_ne!(k_a, k_none, "stream A vs no-stream → different keys");
    }

    #[test]
    fn cache_get_returns_none_on_miss() {
        let cache = AmbientCache::with_config(Duration::from_secs(60), 10);
        let k = build_key("u", "s", None, None, None);
        assert!(cache.get(&k).is_none());
    }

    #[test]
    fn cache_put_then_get_returns_value() {
        let cache = AmbientCache::with_config(Duration::from_secs(60), 10);
        let k = build_key("u", "s", None, None, None);
        cache.put(k, empty_resp());
        assert!(cache.get(&k).is_some());
    }

    #[test]
    fn cache_expires_after_ttl() {
        let cache = AmbientCache::with_config(Duration::from_millis(1), 10);
        let k = build_key("u", "s", None, None, None);
        cache.put(k, empty_resp());
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get(&k).is_none());
    }

    #[test]
    fn cache_evicts_oldest_at_capacity() {
        let cache = AmbientCache::with_config(Duration::from_secs(60), 2);
        let k1 = build_key("u1", "s", None, None, None);
        let k2 = build_key("u2", "s", None, None, None);
        let k3 = build_key("u3", "s", None, None, None);
        cache.put(k1, empty_resp());
        cache.put(k2, empty_resp());
        // Touch k1 to make k2 the LRU candidate.
        let _ = cache.get(&k1);
        cache.put(k3, empty_resp());
        assert!(cache.get(&k1).is_some(), "recently-accessed entry survives");
        assert!(
            cache.get(&k2).is_none(),
            "least-recently-used entry evicted"
        );
        assert!(cache.get(&k3).is_some(), "newly-inserted entry survives");
    }

    #[test]
    fn cache_ttl_env_var_parses() {
        // Note: this test mutates process env and is therefore inherently
        // serial; if a sibling test ever reads the same env, switch to
        // a tokio::sync::Mutex per the env-var test serialization pattern.
        // SAFETY: tests run in single-threaded mode for env mutation.
        unsafe {
            std::env::set_var(CACHE_TTL_ENV, "120");
        }
        let ttl = cache_ttl();
        unsafe {
            std::env::remove_var(CACHE_TTL_ENV);
        }
        assert_eq!(ttl, Duration::from_secs(120));
    }

    #[test]
    fn count_tokens_returns_nonzero_for_nonempty_string() {
        let n = count_tokens("hello world").unwrap();
        assert!(n > 0);
    }

    #[test]
    fn count_tokens_empty_returns_zero() {
        assert_eq!(count_tokens("").unwrap(), 0);
    }

    #[test]
    fn truncate_to_budget_drops_low_tier_first() {
        let snippets = vec![
            snip("Low tier fact one.", Tier::Low, 0.30),
            snip("High tier fact one.", Tier::High, 0.91),
            snip("Low tier fact two.", Tier::Low, 0.20),
        ];
        // Under generous budget every snippet survives — sorting changes order.
        let kept = truncate_to_budget(snippets, None).unwrap();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].tier, Tier::High, "high tier ranks first in result");
    }

    #[test]
    fn truncate_to_budget_drops_oversized_snippets_silently() {
        let huge = "word ".repeat(500); // ~500 tokens — over 200/snippet cap.
        let snippets = vec![
            snip("OK fact.", Tier::High, 0.9),
            snip(&huge, Tier::High, 0.85),
        ];
        let kept = truncate_to_budget(snippets, None).unwrap();
        assert_eq!(kept.len(), 1, "oversized snippet (>200 tok) is dropped");
        assert_eq!(kept[0].text, "OK fact.");
    }

    #[test]
    fn truncate_to_budget_preserves_marker_token_floor() {
        // Marker present: budget = 1500 - marker_tokens. Snippets share the rest.
        let marker = MarkerIfEmpty {
            ambient: NegativeAmbientStatus::NoRelevantContext,
            checked: true,
            scope: "private:u1".to_string(),
            reason: NegativeReason::BelowThreshold,
        };
        let snippets = vec![snip("Fact A.", Tier::High, 0.9)];
        let kept = truncate_to_budget(snippets, Some(&marker)).unwrap();
        assert_eq!(kept.len(), 1, "small snippet fits alongside marker");
    }
}
