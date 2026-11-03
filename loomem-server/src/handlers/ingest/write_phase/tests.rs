use super::*;
use loomem_core::apply_guard::ApplyGuard;

#[test]
fn pending_write_is_revocable_but_durable_write_must_finish() {
    let pending = WritePhase::new();
    assert!(pending.revoke());
    assert!(pending.enter_durable().is_err());

    let durable = WritePhase::new();
    durable.enter_durable().expect("enter durable phase");
    assert!(!durable.revoke());
}

#[tokio::test]
async fn open_apply_section_defers_revocation_to_the_next_checkpoint() {
    let phase = WritePhase::new();
    phase.begin().await.expect("section opens");
    assert!(!phase.revoke(), "an open section must finish");
    assert!(phase.enter_durable().is_err());
    phase.end();
    assert!(WritePhase::checkpoint(&phase).is_err());
    assert!(phase.begin().await.is_err());
    assert!(phase.revoke(), "between sections the task is abortable");
}

#[tokio::test]
async fn sections_track_the_installed_phase() {
    let phase = Arc::new(WritePhase::new());
    track(phase.clone(), async {
        let section = loomem_core::apply_guard::begin_section()
            .await
            .expect("section opens");
        assert!(!WRITE_PHASE.with(|phase| phase.revoke()));
        drop(section);
        assert!(loomem_core::apply_guard::checkpoint().is_err());
    })
    .await;
}
