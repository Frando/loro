//! Checks of remote op contents before the ops reach the oplog.
//!
//! The diff calculators, the trackers and the container states assume that
//! every op is one an honest peer could have produced. The checks here refuse,
//! with an import error and before any mutation, the shapes that break those
//! assumptions and that can be checked without replaying history:
//!
//! - the op content kind matches the container type;
//! - positions and lengths fit the `i32` positions of the trackers, and a
//!   delete span neither starts before 0 nor has length 0;
//! - a style mark has `start < end`, and a `StyleEnd` directly follows a
//!   `StyleStart` of the same container;
//! - an op in a container created by its own peer comes after the op that
//!   created the container, and a created child container has the id of the
//!   op that creates it. Together these keep the container parent links
//!   acyclic within one peer.

use loro_common::{
    ContainerID, ContainerType, Counter, HasCounterSpan, LoroError, LoroResult, LoroValue, ID,
};
use rle::HasLength;

use crate::{
    change::Change,
    container::{list::list_op::InnerListOp, tree::tree_op::TreeOp},
    op::{FutureInnerContent, InnerContent, Op},
    OpLog,
};

fn invalid(id: ID, what: &str) -> LoroError {
    LoroError::DecodeError(format!("invalid op {id}: {what}").into_boxed_str())
}

/// `start + len` as an `i32` position, if it fits.
fn fits_i32(start: usize, len: usize) -> bool {
    start
        .checked_add(len)
        .is_some_and(|end| end <= i32::MAX as usize)
}

impl OpLog {
    /// Checks the op contents of decoded remote changes (see the module docs).
    ///
    /// `changes` are not in the oplog yet, or they are all of it (a snapshot
    /// without state). A `StyleEnd` that starts a change is checked against
    /// the op before it, looked up in `changes` and then in the oplog; if
    /// neither holds it, the change waits for it, and when it arrives the
    /// diff calculator gives such an end no effect.
    pub(crate) fn check_remote_ops(&self, changes: &[Change]) -> LoroResult<()> {
        for (change_index, change) in changes.iter().enumerate() {
            let mut prev: Option<&Op> = None;
            for op in change.ops.iter() {
                let id = ID::new(change.id.peer, op.counter);
                check_op(&self.arena, id, op)?;
                if matches!(op.content, InnerContent::List(InnerListOp::StyleEnd)) {
                    if op.counter == 0 {
                        return Err(invalid(id, "style end without style start"));
                    }
                    let before = ID::new(id.peer, op.counter - 1);
                    let start_ok = match prev {
                        Some(p) => Some(is_style_start_in(p, op)),
                        None => self.style_start_before(changes, change_index, before, op),
                    };
                    if start_ok == Some(false) {
                        return Err(invalid(id, "style end without style start"));
                    }
                }
                if let Some(p) = prev {
                    if matches!(
                        p.content,
                        InnerContent::List(InnerListOp::StyleStart { .. })
                    ) && !(matches!(op.content, InnerContent::List(InnerListOp::StyleEnd))
                        && p.container == op.container)
                    {
                        return Err(invalid(id, "style start without style end"));
                    }
                }
                prev = Some(op);
            }
        }
        Ok(())
    }

    /// Whether the op at `before` is a `StyleStart` in the container of
    /// `end`, or `None` if neither `changes[..change_index]` nor the oplog
    /// holds it.
    fn style_start_before(
        &self,
        changes: &[Change],
        change_index: usize,
        before: ID,
        end: &Op,
    ) -> Option<bool> {
        for c in changes[..change_index].iter().rev() {
            if c.id.peer != before.peer
                || c.id.counter > before.counter
                || c.id.counter + c.atom_len() as Counter <= before.counter
            {
                continue;
            }
            let op = c
                .ops
                .iter()
                .find(|o| o.counter <= before.counter && before.counter < o.ctr_end())?;
            return Some(op.counter == before.counter && is_style_start_in(op, end));
        }
        let op = self.get_op_that_includes(before)?;
        Some(op.counter == before.counter && is_style_start_in(&op, end))
    }
}

fn is_style_start_in(op: &Op, end: &Op) -> bool {
    op.container == end.container
        && matches!(
            op.content,
            InnerContent::List(InnerListOp::StyleStart { .. })
        )
}

fn check_op(arena: &crate::arena::SharedArena, id: ID, op: &Op) -> LoroResult<()> {
    let ty = op.container.get_type();
    let kind_ok = match &op.content {
        InnerContent::List(l) => match l {
            InnerListOp::Insert { .. } => {
                matches!(ty, ContainerType::List | ContainerType::MovableList)
            }
            InnerListOp::Delete(_) => matches!(
                ty,
                ContainerType::List | ContainerType::MovableList | ContainerType::Text
            ),
            InnerListOp::Move { .. } | InnerListOp::Set { .. } => {
                matches!(ty, ContainerType::MovableList)
            }
            InnerListOp::InsertText { .. }
            | InnerListOp::StyleStart { .. }
            | InnerListOp::StyleEnd => matches!(ty, ContainerType::Text),
        },
        InnerContent::Map(_) => matches!(ty, ContainerType::Map),
        InnerContent::Tree(_) => matches!(ty, ContainerType::Tree),
        InnerContent::Future(f) => match f {
            #[cfg(feature = "counter")]
            FutureInnerContent::Counter(_) => matches!(ty, ContainerType::Counter),
            FutureInnerContent::Unknown { .. } => matches!(ty, ContainerType::Unknown(_)),
        },
    };
    if !kind_ok {
        return Err(invalid(id, "op content does not match the container type"));
    }

    match &op.content {
        InnerContent::List(l) => match l {
            InnerListOp::Insert { slice, pos } => {
                if !fits_i32(*pos, slice.atom_len()) {
                    return Err(invalid(id, "position out of range"));
                }
            }
            InnerListOp::InsertText {
                unicode_len, pos, ..
            } => {
                if !fits_i32(*pos as usize, *unicode_len as usize) {
                    return Err(invalid(id, "position out of range"));
                }
            }
            InnerListOp::Delete(del) => {
                let span = del.span;
                let start = if span.signed_len > 0 {
                    Some(span.pos)
                } else {
                    span.pos
                        .checked_add(1)
                        .and_then(|p| p.checked_add(span.signed_len))
                };
                let len = span.signed_len.unsigned_abs();
                let ok = span.signed_len != 0
                    && start.is_some_and(|s| s >= 0 && fits_i32(s as usize, len))
                    && del.id_start.counter >= 0
                    && fits_i32(del.id_start.counter as usize, len);
                if !ok {
                    return Err(invalid(id, "delete span out of range"));
                }
            }
            InnerListOp::Move { from, to, .. } => {
                if !fits_i32(*from as usize, 1) || !fits_i32(*to as usize, 1) {
                    return Err(invalid(id, "position out of range"));
                }
            }
            InnerListOp::StyleStart { start, end, .. } => {
                if start >= end || !fits_i32(*end as usize, 1) {
                    return Err(invalid(id, "style range out of range"));
                }
            }
            InnerListOp::Set { .. } | InnerListOp::StyleEnd => {}
        },
        InnerContent::Map(_) | InnerContent::Future(_) => {}
        InnerContent::Tree(t) => {
            if let TreeOp::Create { target, .. } = t.as_ref() {
                if target.id() != id {
                    return Err(invalid(id, "tree create target must be the op id"));
                }
            }
        }
    }

    // An op in a container created by its own peer must come after the op
    // that created it.
    if let Some(ContainerID::Normal { peer, counter, .. }) = arena.get_container_id(op.container) {
        if peer == id.peer && counter >= id.counter {
            return Err(invalid(id, "op in a container created by a later op"));
        }
    }

    // A created child container has the id of the atom that creates it.
    let mut child_err = None;
    let mut offset = 0;
    let check_child = |value: &LoroValue, offset: Counter, err: &mut Option<LoroError>| {
        if let LoroValue::Container(c) = value {
            let ok = match c {
                ContainerID::Normal { peer, counter, .. } => {
                    *peer == id.peer && Some(*counter) == id.counter.checked_add(offset)
                }
                ContainerID::Root { .. } => false,
            };
            if !ok && err.is_none() {
                *err = Some(invalid(id, "created container id must be the op id"));
            }
        }
    };
    match &op.content {
        InnerContent::List(InnerListOp::Insert { slice, .. }) => {
            for v in arena.iter_value_slice(slice.to_range()) {
                check_child(&v, offset, &mut child_err);
                offset += 1;
            }
        }
        InnerContent::List(InnerListOp::Set { value, .. }) => check_child(value, 0, &mut child_err),
        InnerContent::Map(m) => {
            if let Some(v) = &m.value {
                check_child(v, 0, &mut child_err)
            }
        }
        _ => {}
    }
    if let Some(e) = child_err {
        return Err(e);
    }
    Ok(())
}
