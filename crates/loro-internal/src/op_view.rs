//! Borrowed views of the ops in the oplog, for readers outside the crate.
//!
//! [`OpLog::visit_ops_in_span`] calls a closure per op of a peer's counter
//! span, with the op's change and container, and the op's content
//! borrowed from the change store and the arena. It reads the same ops as
//! [`crate::LoroDoc::export_json_in_id_span`] without building JSON
//! changes, owned strings or value vectors.

use loro_common::{ContainerID, Counter, HasCounterSpan, IdLp, IdSpan, LoroValue, TreeID, ID};

use crate::container::idx::ContainerIdx;
use crate::container::list::list_op::{DeleteSpanWithId, InnerListOp};
use crate::container::map::MapSet;
use crate::container::tree::tree_op::TreeOp;
use crate::encoding::OwnedValue;
use crate::op::{FutureInnerContent, InnerContent};
use crate::oplog::OpLog;
use crate::version::Frontiers;

/// The change an op belongs to.
#[derive(Debug)]
pub struct ChangeView<'a> {
    /// The id of the change's first op.
    pub id: ID,
    /// The change's deps, in no particular order (the JSON export sorts
    /// them).
    pub deps: &'a Frontiers,
    /// The change's commit message.
    pub message: Option<&'a str>,
    /// The change's Lamport time.
    pub lamport: u32,
    /// The change's timestamp.
    pub timestamp: i64,
}

/// The value of an op in a container type this Loro does not know.
#[derive(Debug)]
pub enum UnknownValue<'a> {
    /// A value with a [`LoroValue`] form.
    Value(LoroValue),
    /// A value kind this Loro does not know: its kind and raw bytes.
    Future {
        /// The value kind.
        kind: u8,
        /// The encoded value.
        data: &'a [u8],
    },
    /// An encoding-internal value (arena indices); not canonical.
    Internal,
}

/// The content of one op, borrowed.
#[derive(Debug)]
pub enum OpContentView<'a> {
    /// Text inserted at unicode position `pos`.
    TextInsert {
        /// The unicode position.
        pos: u32,
        /// The inserted text.
        text: &'a str,
    },
    /// Values inserted into a list or movable list.
    ListInsert {
        /// The position.
        pos: u32,
        /// The inserted values.
        values: &'a [LoroValue],
    },
    /// A delete in a text, list or movable list.
    Delete {
        /// The position.
        pos: isize,
        /// The signed length (negative: backward).
        signed_len: isize,
        /// The id of the first deleted element.
        start_id: ID,
    },
    /// The start of a text style.
    Mark {
        /// Start.
        start: u32,
        /// End.
        end: u32,
        /// Style key.
        key: &'a str,
        /// Style value.
        value: &'a LoroValue,
        /// Style info flags.
        info: u8,
    },
    /// The end of a text style.
    MarkEnd,
    /// A move in a movable list.
    Move {
        /// From.
        from: u32,
        /// To.
        to: u32,
        /// The moved element.
        elem: IdLp,
    },
    /// A set in a movable list.
    Set {
        /// The element.
        elem: IdLp,
        /// The new value.
        value: &'a LoroValue,
    },
    /// A map set; `None` deletes.
    MapSet {
        /// Key.
        key: &'a str,
        /// Value.
        value: Option<&'a LoroValue>,
    },
    /// A tree node created.
    TreeCreate {
        /// Node.
        target: TreeID,
        /// Parent, `None` for a root.
        parent: Option<TreeID>,
        /// Fractional index bytes.
        position: &'a [u8],
    },
    /// A tree node moved.
    TreeMove {
        /// Node.
        target: TreeID,
        /// Parent, `None` for a root.
        parent: Option<TreeID>,
        /// Fractional index bytes.
        position: &'a [u8],
    },
    /// A tree node deleted.
    TreeDelete {
        /// Node.
        target: TreeID,
    },
    /// A counter increment.
    Counter(f64),
    /// An op of a container type this Loro does not know.
    Unknown {
        /// The op's prop.
        prop: i32,
        /// The op's value.
        value: UnknownValue<'a>,
    },
}

/// One op, borrowed.
#[derive(Debug)]
pub struct OpView<'a> {
    /// The op's first counter.
    pub counter: Counter,
    /// The number of counters it takes.
    pub len: usize,
    /// Its container.
    pub container: &'a ContainerID,
    /// Its content.
    pub content: OpContentView<'a>,
}

impl OpLog {
    /// Calls `f` for every op that overlaps `span`, in counter order.
    ///
    /// Ops and changes are given whole: the first and the last may start
    /// before or end after the span, and a caller that needs the atoms of
    /// the span only slices them (`OpView::counter` and `OpView::len` give
    /// the op's counter range). The span is clamped to the counters the
    /// oplog holds for its peer, as in `export_json_in_id_span`.
    /// Uncommitted ops are not visited.
    ///
    /// Returns the counter after the last op visited, or the clamped
    /// span's start if none was visited. It is below the clamped span's
    /// end if the change store holds no change at a counter of the span
    /// (an uncommitted change, history before a shallow root), or if an op
    /// could not be read (its container is unknown to the arena or its
    /// text is not UTF-8), which the oplog does not produce.
    pub fn visit_ops_in_span(
        &self,
        mut span: IdSpan,
        f: &mut dyn FnMut(&ChangeView<'_>, &OpView<'_>),
    ) -> Counter {
        span.normalize_();
        let held = self.vv().get(&span.peer).copied().unwrap_or(0);
        span.counter.start = span.counter.start.max(0);
        span.counter.end = span.counter.end.min(held);
        let mut end = span.counter.start;
        if span.counter.start >= span.counter.end {
            return end;
        }
        let mut cached: Option<(ContainerIdx, ContainerID)> = None;
        let mut next = span.counter.start;
        while next < span.counter.end {
            let Some(change) = self.get_change_at(ID::new(span.peer, next)) else {
                break;
            };
            next = change.ctr_end();
            let cv = ChangeView {
                id: change.id,
                deps: &change.deps,
                message: change.commit_msg.as_deref(),
                lamport: change.lamport,
                timestamp: change.timestamp,
            };
            for op in change.ops.iter() {
                let len = rle::HasLength::atom_len(op);
                if op.counter + len as Counter <= span.counter.start {
                    continue;
                }
                if op.counter >= span.counter.end {
                    break;
                }
                if cached.as_ref().is_none_or(|(i, _)| *i != op.container) {
                    let Some(id) = self.arena.get_container_id(op.container) else {
                        return end;
                    };
                    cached = Some((op.container, id));
                }
                let container = &cached.as_ref().unwrap().1;
                let mut call = |content: OpContentView<'_>| {
                    f(
                        &cv,
                        &OpView {
                            counter: op.counter,
                            len,
                            container,
                            content,
                        },
                    )
                };
                match &op.content {
                    InnerContent::List(l) => match l {
                        InnerListOp::InsertText { slice, pos, .. } => {
                            let Ok(text) = std::str::from_utf8(slice.as_bytes()) else {
                                return end;
                            };
                            call(OpContentView::TextInsert { pos: *pos, text });
                        }
                        InnerListOp::Insert { slice, pos } => {
                            let range = slice.0.start as usize..slice.0.end as usize;
                            if container.container_type() == loro_common::ContainerType::Text {
                                self.arena.with_text_slice(range, |text| {
                                    call(OpContentView::TextInsert {
                                        pos: *pos as u32,
                                        text,
                                    })
                                });
                            } else {
                                self.arena.with_values(range, |values| {
                                    call(OpContentView::ListInsert {
                                        pos: *pos as u32,
                                        values,
                                    })
                                });
                            }
                        }
                        InnerListOp::Delete(DeleteSpanWithId { id_start, span }) => {
                            call(OpContentView::Delete {
                                pos: span.pos,
                                signed_len: span.signed_len,
                                start_id: *id_start,
                            })
                        }
                        InnerListOp::Move { from, elem_id, to } => call(OpContentView::Move {
                            from: *from,
                            to: *to,
                            elem: *elem_id,
                        }),
                        InnerListOp::Set { elem_id, value } => call(OpContentView::Set {
                            elem: *elem_id,
                            value,
                        }),
                        InnerListOp::StyleStart {
                            start,
                            end,
                            key,
                            value,
                            info,
                        } => call(OpContentView::Mark {
                            start: *start,
                            end: *end,
                            key,
                            value,
                            info: info.to_byte(),
                        }),
                        InnerListOp::StyleEnd => call(OpContentView::MarkEnd),
                    },
                    InnerContent::Map(MapSet { key, value }) => call(OpContentView::MapSet {
                        key,
                        value: value.as_ref(),
                    }),
                    InnerContent::Tree(t) => match &**t {
                        TreeOp::Create {
                            target,
                            parent,
                            position,
                        } => call(OpContentView::TreeCreate {
                            target: *target,
                            parent: *parent,
                            position: position.as_bytes(),
                        }),
                        TreeOp::Move {
                            target,
                            parent,
                            position,
                        } => call(OpContentView::TreeMove {
                            target: *target,
                            parent: *parent,
                            position: position.as_bytes(),
                        }),
                        TreeOp::Delete { target } => {
                            call(OpContentView::TreeDelete { target: *target })
                        }
                    },
                    InnerContent::Future(fc) => match fc {
                        #[cfg(feature = "counter")]
                        FutureInnerContent::Counter(x) => call(OpContentView::Counter(*x)),
                        FutureInnerContent::Unknown { prop, value } => {
                            let value = match &**value {
                                OwnedValue::Null => UnknownValue::Value(LoroValue::Null),
                                OwnedValue::True => UnknownValue::Value(LoroValue::Bool(true)),
                                OwnedValue::False => UnknownValue::Value(LoroValue::Bool(false)),
                                OwnedValue::I64(i) => UnknownValue::Value(LoroValue::I64(*i)),
                                OwnedValue::F64(x) => UnknownValue::Value(LoroValue::Double(*x)),
                                OwnedValue::Str(s) => {
                                    UnknownValue::Value(LoroValue::from(s.as_str()))
                                }
                                OwnedValue::Binary(b) => {
                                    UnknownValue::Value(LoroValue::from(b.to_vec()))
                                }
                                OwnedValue::LoroValue(v) => UnknownValue::Value(v.clone()),
                                OwnedValue::Future(
                                    crate::encoding::value::OwnedFutureValue::Unknown {
                                        kind,
                                        data,
                                    },
                                ) => UnknownValue::Future { kind: *kind, data },
                                _ => UnknownValue::Internal,
                            };
                            call(OpContentView::Unknown { prop: *prop, value })
                        }
                    },
                }
                end = op.counter + len as Counter;
            }
        }
        end
    }
}
