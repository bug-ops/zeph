// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end regression for issue #6462's decisive security-audit finding: once the
//! reconcile-and-cap sweep has *stamped* (not reaped) an orphaned anchor, a file recreated under
//! that still-anchored identity within the grace window must still be caught as TAMPER by the
//! real read path — `zeph_subagent::transcript::verify_and_extract_messages` /
//! `zeph_session::log`'s equivalent — not just by the unit-level `run_anchor_sweep` assertions in
//! `zeph_core::anchor_store`'s own test module.
//!
//! Exercises the real `AgeVaultAnchorStore` over a real on-disk age vault (not the in-memory
//! `MockAnchorStore` the `zeph-subagent`/`zeph-session` unit tests use), the real
//! `run_anchor_sweep`, and the real `TranscriptWriter`/`TranscriptReader` and `SessionEventLog`
//! read/write paths — this is the only crate with a production dependency on all three pieces.

use std::path::Path;
use std::sync::{Arc, RwLock as StdRwLock};

use tokio_util::sync::CancellationToken;

use zeph_common::anchor::AnchorStore;
use zeph_common::hash_chain::{ChainKey, ChainKeyRing};
use zeph_common::task_supervisor::TaskSupervisor;
use zeph_core::anchor_store::{AgeVaultAnchorStore, run_anchor_sweep};
use zeph_core::vault::AgeVaultProvider;
use zeph_llm::provider::{Message, MessageMetadata, Role};

fn test_vault(dir: &Path) -> Arc<StdRwLock<AgeVaultProvider>> {
    AgeVaultProvider::init_vault(dir).unwrap();
    let provider =
        AgeVaultProvider::load(&dir.join("vault-key.txt"), &dir.join("secrets.age")).unwrap();
    Arc::new(StdRwLock::new(provider))
}

fn test_message(role: Role, content: &str) -> Message {
    Message {
        role,
        content: content.to_owned(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }
}

/// Strip every `"chain"` field from a JSONL blob, reproducing a whole-file downgrade strip —
/// same technique `zeph-subagent`/`zeph-session`'s own whole-strip unit tests use.
fn strip_chain_fields(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            value.as_object_mut().unwrap().remove("chain");
            value.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[tokio::test]
async fn recreated_legacy_transcript_under_grace_stamped_anchor_is_still_tamper() {
    use zeph_subagent::transcript::{TranscriptReader, TranscriptWriter, configure_anchor_store};

    let dir = tempfile::tempdir().unwrap();
    let vault = test_vault(dir.path());
    let transcript_dir = dir.path().join("transcripts");
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    std::fs::create_dir_all(&sessions_dir).unwrap();

    let ring = Arc::new(ChainKeyRing::new(0, ChainKey::new([7u8; 32])));
    zeph_subagent::transcript::configure_history_integrity(Some(Arc::clone(&ring)));
    let supervisor = TaskSupervisor::new(CancellationToken::new());
    let store: Arc<dyn AnchorStore> =
        Arc::new(AgeVaultAnchorStore::new(Arc::clone(&vault), supervisor));
    configure_anchor_store(Some(Arc::clone(&store)));

    // Write a real, chained, finalized transcript — `finalize()` writes the anchor through the
    // real vault, not a mock.
    let path = transcript_dir.join("task1.jsonl");
    let writer = TranscriptWriter::new(&path).unwrap();
    writer
        .append(0, &test_message(Role::User, "one"))
        .await
        .unwrap();
    writer
        .append(1, &test_message(Role::Assistant, "two"))
        .await
        .unwrap();
    writer.finalize().await.unwrap();

    // Sanity: opens fine while untouched.
    assert_eq!(TranscriptReader::load(&path).unwrap().len(), 2);

    let stripped = strip_chain_fields(&std::fs::read_to_string(&path).unwrap());

    // The real file disappears (attacker delete, or accidental loss) — the scenario the #6462
    // grace window was built for.
    std::fs::remove_file(&path).unwrap();

    // First sweep observes the absence: stamps `orphaned_since`, does NOT reap.
    let t0 = 10_000_000u64;
    let report = run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, t0).unwrap();
    assert_eq!(
        report.orphans_stamped, 1,
        "first sweep must stamp, not reap"
    );
    assert_eq!(report.orphans_reaped, 0);

    // A forged legacy-looking replacement is recreated under the exact same identity — the
    // attacker has file-write access only, never the vault, so this simulates a raw write, not a
    // legitimate `TranscriptWriter::finalize()` (which would require the anchor store).
    std::fs::write(&path, &stripped).unwrap();

    // Decisive assertion: the anchor is still present (merely stamped, not reaped) — the real
    // read path must still catch this as TAMPER, exactly as it would have before #6462.
    let err = TranscriptReader::load(&path).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("TAMPER") && msg.contains("vault anchor"),
        "expected TAMPER-with-vault-anchor error, got: {msg}"
    );

    configure_anchor_store(None);
    zeph_subagent::transcript::configure_history_integrity(None);
}

#[tokio::test]
async fn recreated_legacy_session_under_grace_stamped_anchor_is_still_tamper() {
    use zeph_session::SessionEventLog;
    use zeph_session::log::configure_anchor_store;

    let dir = tempfile::tempdir().unwrap();
    let vault = test_vault(dir.path());
    let transcript_dir = dir.path().join("transcripts");
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    std::fs::create_dir_all(&sessions_dir).unwrap();

    let ring = Arc::new(ChainKeyRing::new(0, ChainKey::new([8u8; 32])));
    zeph_session::log::configure_history_integrity(Some(Arc::clone(&ring)));
    let supervisor = TaskSupervisor::new(CancellationToken::new());
    let store: Arc<dyn AnchorStore> =
        Arc::new(AgeVaultAnchorStore::new(Arc::clone(&vault), supervisor));
    configure_anchor_store(Some(Arc::clone(&store)));

    let session_id = "sess1";
    let session_path = zeph_session::session_dir(&sessions_dir, session_id);
    let log = SessionEventLog::open(&session_path).await.unwrap();
    log.append(
        None,
        None,
        zeph_session::SessionEvent::UserMessage {
            text: "one".to_owned(),
            image_refs: vec![],
        },
    )
    .await
    .unwrap();
    log.append(
        None,
        None,
        zeph_session::SessionEvent::SessionEnded { reason: "x".into() },
    )
    .await
    .unwrap();
    log.finalize().await.unwrap();
    drop(log);

    // Sanity: opens fine while untouched.
    assert!(SessionEventLog::open(&session_path).await.is_ok());

    let events_path = session_path.join("events.jsonl");
    let stripped = strip_chain_fields(&tokio::fs::read_to_string(&events_path).await.unwrap());

    // The real session directory disappears entirely — `run_anchor_sweep`'s SessionLog existence
    // check is on the directory (`zeph_session::session_dir(..).exists()`), not just the file.
    tokio::fs::remove_dir_all(&session_path).await.unwrap();

    let t0 = 20_000_000u64;
    let report = run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, t0).unwrap();
    assert_eq!(
        report.orphans_stamped, 1,
        "first sweep must stamp, not reap"
    );
    assert_eq!(report.orphans_reaped, 0);

    // Recreate a forged legacy-looking session under the same identity — raw filesystem writes
    // only, never through a legitimate `SessionEventLog::finalize()`.
    tokio::fs::create_dir_all(&session_path).await.unwrap();
    tokio::fs::write(&events_path, &stripped).await.unwrap();

    match SessionEventLog::open(&session_path).await {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("TAMPER") && msg.contains("vault anchor"),
                "expected TAMPER-with-vault-anchor error, got: {msg}"
            );
        }
        Ok(_) => panic!("expected the still-stamped anchor to catch the recreated legacy session"),
    }

    configure_anchor_store(None);
    zeph_session::log::configure_history_integrity(None);
}
