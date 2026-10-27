//! A document whose lock was poisoned by a panic (for example inside an
//! import of crafted bytes) can be dropped without a second panic. During
//! the unwind such a panic aborts the process, which `catch_unwind` in the
//! caller cannot contain.

use std::panic::{catch_unwind, AssertUnwindSafe};

use loro_internal::{cursor::PosType, LoroDoc};

/// An auto-commit document with an uncommitted insert, so that dropping it
/// commits.
fn doc_with_pending_ops() -> LoroDoc {
    let doc = LoroDoc::new_auto_commit();
    doc.get_text("t").insert(0, "a", PosType::Unicode).unwrap();
    doc
}

#[test]
fn a_doc_with_a_poisoned_oplog_drops() {
    let doc = doc_with_pending_ops();
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _oplog = doc.oplog().lock();
        panic!("crafted input");
    }));
    assert!(caught.is_err());
    assert!(doc.oplog().is_poisoned());
    drop(doc);
}

#[test]
fn a_doc_with_a_poisoned_state_drops() {
    let doc = LoroDoc::new_auto_commit();
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _state = doc.app_state().lock();
        panic!("crafted input");
    }));
    assert!(caught.is_err());
    assert!(doc.app_state().is_poisoned());
    drop(doc);
}

/// The document is owned inside the closure, so it drops during the
/// unwind, after the guard that poisons its oplog.
#[test]
fn a_doc_dropped_during_the_unwind_does_not_abort() {
    let caught = catch_unwind(|| {
        let doc = doc_with_pending_ops();
        let _oplog = doc.oplog().lock();
        panic!("crafted input");
    });
    assert!(caught.is_err());
}
