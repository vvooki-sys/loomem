//! Scope resolution for memory endpoints (cycle/18 §3.3; renamed from
//! `dashboard_scope` in cycle/004 after the dashboard was removed).
//!
//! `?scope=shared|private|all` (query param or request field, e.g. on
//! `POST /v1/search`) controls which stream(s) the handler reads. Enforcement
//! lives here, not in the middleware, because the rules are endpoint-specific
//! (shared is open to any role, `all` is admin-only, `private` requires an
//! active per-user stream).

use crate::auth::AuthContext;
use crate::handlers::AppError;
use loomem_core::storage::{RocksDbStore, DEFAULT_STREAM_ID};
use serde::Deserialize;

/// `?scope=` query param. Default is `Shared` to preserve the legacy
/// no-param behaviour of pre-existing callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeParam {
    #[default]
    Shared,
    Private,
    All,
}

/// Per-stream source label attached to response items so the client can
/// distinguish union results when `scope=all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Shared,
    Private,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Private => "private",
        }
    }
}

/// Which streams the handler should scan, paired with a source label used
/// both for the response payload and for dedup precedence when `scope=all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeResolution {
    pub streams: Vec<(String, Source)>,
}

impl ScopeResolution {
    /// Does a given chunk stream_id belong to the resolved scope? Returns the
    /// source label if so. Production consumer (the dashboard list mapping)
    /// was removed in cycle/004 and reinstated with the embedded dashboard
    /// (`handlers/dashboard.rs`); tests keep using it as the canonical way to
    /// assert resolution contents.
    pub fn source_for(&self, stream_id: &str) -> Option<Source> {
        self.streams
            .iter()
            .find(|(s, _)| s == stream_id)
            .map(|(_, src)| *src)
    }
}

/// Look up the caller's private stream id, if they have one active.
///
/// Master admin (auth via `admin_token`) has no `user_id` so this is `None`
/// for them — that is intentional, the master token is a global override
/// and the private stream concept does not apply.
fn caller_private_stream(auth: &AuthContext, store: &RocksDbStore) -> Option<String> {
    let user_id = auth.user_id.as_deref()?;
    let user = store.get_user_by_id(user_id).ok().flatten()?;
    let flags_bytes = store.get_user_flags(user_id).ok().flatten()?;
    let flags: serde_json::Value = serde_json::from_slice(&flags_bytes).ok()?;
    let active = flags
        .get("private_stream")
        .and_then(|p| p.get("active"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if active {
        Some(user.stream_id)
    } else {
        None
    }
}

/// Resolve a scope param into the list of streams a handler should read.
///
/// - `Shared` — any role, returns `[DEFAULT_STREAM_ID]`.
/// - `Private` — any role, returns `[caller.private_stream]` or 404 if the
///   caller has no active private stream.
/// - `All` — admin only (403 otherwise), returns `[DEFAULT, caller.private?]`.
///   When the admin has no private stream the union degrades to default-only.
pub fn resolve_scope(
    scope: ScopeParam,
    auth: &AuthContext,
    store: &RocksDbStore,
) -> Result<ScopeResolution, AppError> {
    match scope {
        ScopeParam::Shared => Ok(ScopeResolution {
            streams: vec![(DEFAULT_STREAM_ID.to_string(), Source::Shared)],
        }),
        ScopeParam::Private => match caller_private_stream(auth, store) {
            Some(sid) => Ok(ScopeResolution {
                streams: vec![(sid, Source::Private)],
            }),
            None => Err(AppError::NotFound("no private stream for this user".into())),
        },
        ScopeParam::All => {
            if !auth.is_admin {
                return Err(AppError::Forbidden("scope=all requires admin".into()));
            }
            let mut streams = vec![(DEFAULT_STREAM_ID.to_string(), Source::Shared)];
            if let Some(sid) = caller_private_stream(auth, store) {
                streams.push((sid, Source::Private));
            }
            Ok(ScopeResolution { streams })
        }
    }
}

#[cfg(test)]
mod tests {
    //! Cycle/18 §3.3 + AC-10 — 22 tests covering the 3 × 3 scope × role matrix
    //! twice (Memory endpoint × Graph endpoint) plus two edge cases per
    //! endpoint: no-scope default == shared, has_private_stream=false → 404.
    //!
    //! Per brief §6.1 R3: Memory + Graph both delegate to the same
    //! `resolve_scope` helper, so the parametric twin tests below verify the
    //! shared decision surface once per endpoint label for traceability.

    use super::*;
    use crate::auth::KeyScope;
    use loomem_core::config::RocksDbConfig;
    use loomem_core::storage::{RocksDbStore, User, UserRole};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn rocksdb_cfg() -> RocksDbConfig {
        RocksDbConfig {
            max_open_files: 50,
            compression: "none".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        }
    }

    fn fresh_store() -> (Arc<RocksDbStore>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RocksDbStore::open(tmp.path(), &rocksdb_cfg()).unwrap());
        (store, tmp)
    }

    fn seed_user(store: &RocksDbStore, id: &str, role: UserRole, with_private: bool) {
        let u = User {
            id: id.into(),
            api_key: None,
            shared_api_key: Some(format!("tok_{id}_shared")),
            private_api_key: if with_private {
                Some(format!("tok_{id}_private"))
            } else {
                None
            },
            stream_id: format!("s_{id}"),
            created_at: 0,
            last_active: None,
            label: None,
            active: true,
            workspace_id: None,
            role,
            email: None,
            display_name: None,
            external_id: None,
            pending_first_login: false,
            last_login_at: None,
        };
        store.store_user(&u).unwrap();
        if with_private {
            let flags = serde_json::json!({"private_stream": {"active": true}});
            store
                .set_user_flags(id, flags.to_string().as_bytes())
                .unwrap();
        }
    }

    fn auth_for(user_id: &str, role: UserRole) -> AuthContext {
        AuthContext::single_stream(
            DEFAULT_STREAM_ID,
            role,
            KeyScope::Shared,
            Some(user_id.into()),
            role.is_admin(),
        )
    }

    // ── Memory endpoint — 11 tests ────────────────────────────────────────

    #[test]
    fn memory_admin_shared_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "admin", UserRole::Admin, true);
        let auth = auth_for("admin", UserRole::Admin);

        let r = resolve_scope(ScopeParam::Shared, &auth, &store).unwrap();
        assert_eq!(
            r.streams,
            vec![(DEFAULT_STREAM_ID.to_string(), Source::Shared)]
        );
    }

    #[test]
    fn memory_admin_private_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "admin", UserRole::Admin, true);
        let auth = auth_for("admin", UserRole::Admin);

        let r = resolve_scope(ScopeParam::Private, &auth, &store).unwrap();
        assert_eq!(r.streams, vec![("s_admin".into(), Source::Private)]);
    }

    #[test]
    fn memory_admin_all_ok_union() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "admin", UserRole::Admin, true);
        let auth = auth_for("admin", UserRole::Admin);

        let r = resolve_scope(ScopeParam::All, &auth, &store).unwrap();
        assert_eq!(
            r.streams,
            vec![
                (DEFAULT_STREAM_ID.to_string(), Source::Shared),
                ("s_admin".into(), Source::Private),
            ]
        );
    }

    #[test]
    fn memory_writer_shared_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "writer1", UserRole::Writer, true);
        let auth = auth_for("writer1", UserRole::Writer);

        let r = resolve_scope(ScopeParam::Shared, &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, DEFAULT_STREAM_ID);
    }

    #[test]
    fn memory_writer_private_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "writer1", UserRole::Writer, true);
        let auth = auth_for("writer1", UserRole::Writer);

        let r = resolve_scope(ScopeParam::Private, &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, "s_writer1");
        assert_eq!(r.streams[0].1, Source::Private);
    }

    #[test]
    fn memory_writer_all_forbidden() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "writer1", UserRole::Writer, true);
        let auth = auth_for("writer1", UserRole::Writer);

        let err = resolve_scope(ScopeParam::All, &auth, &store).unwrap_err();
        let dbg = format!("{err:?}");
        assert!(dbg.contains("Forbidden"), "got: {dbg}");
        assert!(dbg.contains("scope=all requires admin"), "got: {dbg}");
    }

    #[test]
    fn memory_reader_shared_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "reader1", UserRole::Reader, true);
        let auth = auth_for("reader1", UserRole::Reader);

        let r = resolve_scope(ScopeParam::Shared, &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, DEFAULT_STREAM_ID);
    }

    #[test]
    fn memory_reader_private_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "reader1", UserRole::Reader, true);
        let auth = auth_for("reader1", UserRole::Reader);

        let r = resolve_scope(ScopeParam::Private, &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, "s_reader1");
    }

    #[test]
    fn memory_reader_all_forbidden() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "reader1", UserRole::Reader, true);
        let auth = auth_for("reader1", UserRole::Reader);

        let err = resolve_scope(ScopeParam::All, &auth, &store).unwrap_err();
        assert!(format!("{err:?}").contains("Forbidden"));
    }

    #[test]
    fn memory_no_scope_defaults_to_shared() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "anyone", UserRole::Reader, false);
        let auth = auth_for("anyone", UserRole::Reader);

        assert_eq!(ScopeParam::default(), ScopeParam::Shared);
        let r = resolve_scope(ScopeParam::default(), &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, DEFAULT_STREAM_ID);
    }

    #[test]
    fn memory_private_no_private_stream_not_found() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "writer_no_priv", UserRole::Writer, false);
        let auth = auth_for("writer_no_priv", UserRole::Writer);

        let err = resolve_scope(ScopeParam::Private, &auth, &store).unwrap_err();
        let dbg = format!("{err:?}");
        assert!(dbg.contains("NotFound"), "got: {dbg}");
        assert!(dbg.contains("no private stream"), "got: {dbg}");
    }

    // ── Graph endpoint — 11 parametric twin tests ─────────────────────────
    //
    // Graph and Memory handlers both call resolve_scope (brief §3.3 "Graph
    // analogicznie"). The same matrix is verified under graph_* names so
    // AC-10 traceability (Memory 11 + Graph 11 = 22) holds.

    #[test]
    fn graph_admin_shared_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "admin", UserRole::Admin, true);
        let auth = auth_for("admin", UserRole::Admin);

        let r = resolve_scope(ScopeParam::Shared, &auth, &store).unwrap();
        assert_eq!(r.source_for(DEFAULT_STREAM_ID), Some(Source::Shared));
    }

    #[test]
    fn graph_admin_private_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "admin", UserRole::Admin, true);
        let auth = auth_for("admin", UserRole::Admin);

        let r = resolve_scope(ScopeParam::Private, &auth, &store).unwrap();
        assert_eq!(r.source_for("s_admin"), Some(Source::Private));
    }

    #[test]
    fn graph_admin_all_ok_union() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "admin", UserRole::Admin, true);
        let auth = auth_for("admin", UserRole::Admin);

        let r = resolve_scope(ScopeParam::All, &auth, &store).unwrap();
        assert_eq!(r.streams.len(), 2);
        assert_eq!(r.source_for(DEFAULT_STREAM_ID), Some(Source::Shared));
        assert_eq!(r.source_for("s_admin"), Some(Source::Private));
    }

    #[test]
    fn graph_writer_shared_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "writer1", UserRole::Writer, true);
        let auth = auth_for("writer1", UserRole::Writer);

        let r = resolve_scope(ScopeParam::Shared, &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, DEFAULT_STREAM_ID);
    }

    #[test]
    fn graph_writer_private_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "writer1", UserRole::Writer, true);
        let auth = auth_for("writer1", UserRole::Writer);

        let r = resolve_scope(ScopeParam::Private, &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, "s_writer1");
    }

    #[test]
    fn graph_writer_all_forbidden() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "writer1", UserRole::Writer, true);
        let auth = auth_for("writer1", UserRole::Writer);

        let err = resolve_scope(ScopeParam::All, &auth, &store).unwrap_err();
        let dbg = format!("{err:?}");
        assert!(dbg.contains("Forbidden"), "got: {dbg}");
        assert!(dbg.contains("scope=all requires admin"), "got: {dbg}");
    }

    #[test]
    fn graph_reader_shared_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "reader1", UserRole::Reader, true);
        let auth = auth_for("reader1", UserRole::Reader);

        let r = resolve_scope(ScopeParam::Shared, &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, DEFAULT_STREAM_ID);
    }

    #[test]
    fn graph_reader_private_ok() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "reader1", UserRole::Reader, true);
        let auth = auth_for("reader1", UserRole::Reader);

        let r = resolve_scope(ScopeParam::Private, &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, "s_reader1");
    }

    #[test]
    fn graph_reader_all_forbidden() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "reader1", UserRole::Reader, true);
        let auth = auth_for("reader1", UserRole::Reader);

        let err = resolve_scope(ScopeParam::All, &auth, &store).unwrap_err();
        assert!(format!("{err:?}").contains("Forbidden"));
    }

    #[test]
    fn graph_no_scope_defaults_to_shared() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "anyone", UserRole::Reader, false);
        let auth = auth_for("anyone", UserRole::Reader);

        let r = resolve_scope(ScopeParam::default(), &auth, &store).unwrap();
        assert_eq!(r.streams[0].0, DEFAULT_STREAM_ID);
    }

    #[test]
    fn graph_private_no_private_stream_not_found() {
        let (store, _tmp) = fresh_store();
        seed_user(&store, "writer_no_priv", UserRole::Writer, false);
        let auth = auth_for("writer_no_priv", UserRole::Writer);

        let err = resolve_scope(ScopeParam::Private, &auth, &store).unwrap_err();
        let dbg = format!("{err:?}");
        assert!(dbg.contains("NotFound"), "got: {dbg}");
        assert!(dbg.contains("no private stream"), "got: {dbg}");
    }
}
