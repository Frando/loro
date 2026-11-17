use std::sync::Arc;

use crate::sync::Mutex;
use crate::{
    change::{Change, Timestamp},
    oplog::get_timestamp_now_txn,
    ChangeMeta,
};
use loro_common::PeerID;

/// The callback of the first commit from a peer.
pub type FirstCommitFromPeerCallback =
    Box<dyn Fn(&FirstCommitFromPeerPayload) -> bool + Send + Sync + 'static>;
pub type PreCommitCallback = Box<dyn Fn(&PreCommitCallbackPayload) -> bool + Send + Sync + 'static>;

/// The payload of the pre commit callback.
#[derive(Debug, Clone)]
pub struct PreCommitCallbackPayload {
    /// The metadata of the change which will be committed.
    pub change_meta: ChangeMeta,
    /// The origin of the commit.
    pub origin: String,
    /// The modifier of the change. You can modify the change in the callback.
    pub modifier: ChangeModifier,
}

/// The payload of the first commit from a peer callback.
#[derive(Debug, Clone)]
pub struct FirstCommitFromPeerPayload {
    /// The peer id of the first commit.
    pub peer: PeerID,
}

#[derive(Debug, Clone, Default)]
pub struct ChangeModifier(Arc<Mutex<ChangeModifierInner>>);

#[derive(Debug, Default)]
struct ChangeModifierInner {
    new_msg: Option<Arc<str>>,
    new_timestamp: Option<Timestamp>,
}

impl ChangeModifier {
    pub fn set_message(&self, msg: &str) -> &Self {
        self.0.lock().new_msg = Some(Arc::from(msg));
        self
    }

    pub fn set_timestamp(&self, timestamp: Timestamp) -> &Self {
        self.0.lock().new_timestamp = Some(timestamp);
        self
    }

    pub fn set_timestamp_now(&self) -> &Self {
        self.0.lock().new_timestamp = Some(get_timestamp_now_txn());
        self
    }

    pub(crate) fn modify_change(&self, change: &mut Change) {
        let m = self.0.lock();
        if let Some(msg) = &m.new_msg {
            change.commit_msg = Some(msg.clone());
        }

        if let Some(timestamp) = m.new_timestamp {
            change.timestamp = timestamp;
        }
    }
}

/// The check a checked local commit must pass, set with `LoroDoc::set_commit_check`.
///
/// Returns false to refuse the commit: its ops are undone and never become a
/// change. It runs while the doc's transaction lock is held, so it may read
/// the doc (whose state already shows the ops) but not edit it.
pub type CommitCheckFn = Arc<dyn Fn(&CommitCheck<'_>) -> bool + Send + Sync + 'static>;

/// The pending ops of a checked local commit, given to a [`CommitCheckFn`].
#[derive(Debug)]
pub struct CommitCheck<'a> {
    /// The origin of the commit (`"undo"` for an undo or redo).
    pub origin: &'a str,
    /// The change the ops would form: its first id, deps and Lamport time.
    pub change: crate::op_view::ChangeView<'a>,
    pub(crate) ops: &'a [crate::op::Op],
    pub(crate) range: std::ops::Range<loro_common::Counter>,
    pub(crate) arena: &'a crate::arena::SharedArena,
}

impl CommitCheck<'_> {
    /// Calls `f` for every pending op, in counter order.
    ///
    /// Returns false if an op could not be read.
    pub fn visit_ops(&self, f: &mut dyn FnMut(&crate::op_view::OpView<'_>)) -> bool {
        let mut end = 0;
        crate::op_view::visit_ops(
            self.arena,
            &self.change,
            self.ops.iter(),
            self.range.clone(),
            &mut None,
            &mut end,
            &mut |_, op| f(op),
        )
    }
}
