//! # Encode
//!
//! ```log
//! ≈4KB after compression
//!
//!  N = Number of Changes
//!
//!  Peer_1 = This Peer
//!
//!
//! ┌────────────────────────┬─────────────────────────────────────┐
//! │ LEB Counter Start&Len  │  LEB Lamport Start&Len              │◁───┐
//! └────────────────────────┴─────────────────────────────────────┘    │
//! ┌──────────────┬──────────────┬────────────────────────────────┐    │
//! │    LEB N     │ LEB Peer Num │      8B Peer_1,...,Peer_x      │◁───┤
//! └──────────────┴──────────────┴────────────────────────────────┘    │
//! ┌──────────────────────────────────────────────────────────────┐    │
//! │                   N LEB128 Change AtomLen                    │◁───┼─────  Important metadata
//! └──────────────────────────────────────────────────────────────┘    │
//! ┌───────────────────┬────────────────────────┬─────────────────┐    │
//! │N DepOnSelf BoolRle│ N Delta Rle Deps Lens  │     Dep IDs     │◁───┤
//! └───────────────────┴────────────────────────┴─────────────────┘    │
//! ┌──────────────────────────────────────────────────────────────┐    │
//! │                   N LEB128 Delta Lamports                    │◁───┘
//! └──────────────────────────────────────────────────────────────┘
//! ┌──────────────────────────────────────────────────────────────┐
//! │               N LEB128 DeltaOfDelta Timestamps               │
//! └──────────────────────────────────────────────────────────────┘
//! ┌────────────────────────────────┬─────────────────────────────┐
//! │    N Rle Commit Msg Lengths    │       Commit Messages       │
//! └────────────────────────────────┴─────────────────────────────┘
//!
//!  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ Encoded Operations ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
//!
//! ┌────────────────────┬─────────────────────────────────────────┐
//! │ ContainerIDs Size  │             ContainerIDs                │
//! └────────────────────┴─────────────────────────────────────────┘
//! ┌────────────────────┬─────────────────────────────────────────┐
//! │  Key Strings Size  │               Key Strings               │
//! └────────────────────┴─────────────────────────────────────────┘
//! ┌────────────────────┬─────────────────────────────────────────┐
//! │  Position Size     │               Position                  │
//! └────────────────────┴─────────────────────────────────────────┘
//! ┌────────┬──────────┬──────────┬───────┬───────────────────────┐
//! │        │          │          │       │                       │
//! │        │          │          │       │                       │
//! │        │          │          │       │                       │
//! │  Ops   │  LEB128  │   RLE    │ Delta │                       │
//! │  Size  │ Lengths  │Containers│  RLE  │       ValueType       │
//! │        │          │          │ Props │                       │
//! │        │          │          │       │                       │
//! │        │          │          │       │                       │
//! │        │          │          │       │                       │
//! └────────┴──────────┴──────────┴───────┴───────────────────────┘
//! ┌──────────────────────────────────────────────────────────────┐
//! │             (Encoded with Ops By serde_columnar)             │
//! │                       Delete Start IDs                       │
//! │                                                              │
//! └──────────────────────────────────────────────────────────────┘
//! ┌────────────────┬─────────────────────────────────────────────┐
//! │                │                                             │
//! │Value Bytes Size│                Value Bytes                  │
//! │                │                                             │
//! └────────────────┴─────────────────────────────────────────────┘
//! ```

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;

use fractional_index::FractionalIndex;
use loro_common::{
    ContainerID, Counter, HasCounterSpan, HasLamportSpan, InternalString, Lamport, LoroError,
    LoroResult, PeerID, TreeID, ID,
};
use once_cell::sync::OnceCell;
use rle::HasLength;
use serde::{Deserialize, Serialize};
use serde_columnar::{columnar, AnyRleDecoder, ColumnarError, DeltaOfDeltaDecoder, Itertools};
use tracing::info;

use super::block_meta_encode::decode_changes_header;
use crate::arena::SharedArena;
use crate::change::{Change, Timestamp};
use crate::container::tree::tree_op;
use crate::encoding::arena::{ContainerArena, PositionArena};
use crate::encoding::value_register::ValueRegister;
use crate::encoding::{
    self, decode_op, encode_op, get_op_prop, EncodedDeleteStartId, ImportBudget,
    IterableEncodedDeleteStartId,
};
use crate::op::{InnerContent, Op};

#[derive(Debug, Serialize, Deserialize)]
struct EncodedBlock<'a> {
    counter_start: u32,
    counter_len: u32,
    lamport_start: u32,
    lamport_len: u32,
    n_changes: u32,
    #[serde(borrow)]
    header: Cow<'a, [u8]>,
    // timestamp and commit messages
    #[serde(borrow)]
    change_meta: Cow<'a, [u8]>,
    // ---------------------- Ops ----------------------
    #[serde(borrow)]
    cids: Cow<'a, [u8]>,
    #[serde(borrow)]
    keys: Cow<'a, [u8]>,
    #[serde(borrow)]
    positions: Cow<'a, [u8]>,
    #[serde(borrow)]
    ops: Cow<'a, [u8]>,
    #[serde(borrow)]
    delete_start_ids: Cow<'a, [u8]>,
    #[serde(borrow)]
    values: Cow<'a, [u8]>,
}

fn diagnose_block(block: &EncodedBlock) {
    info!("Diagnosing EncodedBlock:");
    info!("  header {} bytes", block.header.len());
    info!("  change_meta {} bytes", block.change_meta.len());
    info!("  cids: {} bytes", block.cids.len());
    info!("  keys: {} bytes", block.keys.len());
    info!("  positions: {} bytes", block.positions.len());
    info!("  ops: {} bytes", block.ops.len());
    info!("  delete_id_starts: {} bytes", block.delete_start_ids.len());
    info!("  values: {} bytes", block.values.len());
}

const VERSION: u16 = 0;

// MARK: encode_block
/// It's assume that every change in the block share the same peer.
pub fn encode_block(block: &[Change], arena: &SharedArena) -> Vec<u8> {
    if block.is_empty() {
        panic!("Empty block")
    }

    let mut peer_register: ValueRegister<PeerID> = ValueRegister::new();
    let peer = block[0].peer();
    peer_register.register(&peer);

    let cid_register: ValueRegister<ContainerID> = ValueRegister::new();

    let mut encoded_ops = Vec::new();
    let mut registers = Registers {
        peer_register,
        key_register: ValueRegister::new(),
        cid_register,
        position_register: ValueRegister::new(),
    };

    {
        // Init position register, making it ordered by fractional index
        let mut position_set = BTreeSet::default();
        for c in block {
            for op in c.ops().iter() {
                if let crate::op::InnerContent::Tree(tree_op) = &op.content {
                    match &**tree_op {
                        tree_op::TreeOp::Create { position, .. } => {
                            position_set.insert(position.clone());
                        }
                        tree_op::TreeOp::Move { position, .. } => {
                            position_set.insert(position.clone());
                        }
                        tree_op::TreeOp::Delete { .. } => {}
                    }
                }
            }
        }

        for position in position_set {
            registers.position_register.register(&position);
        }
    }

    let mut del_starts: Vec<_> = Vec::new();
    let mut value_writer = ValueWriter::new();
    for c in block {
        for op in c.ops().iter() {
            let cid = arena.get_container_id(op.container).unwrap();
            let cidx = registers.cid_register.register(&cid);
            let prop = get_op_prop(op, &mut registers);
            let value_kind = encode_op(
                op,
                arena,
                &mut del_starts,
                &mut value_writer,
                &mut registers,
            );
            encoded_ops.push(EncodedOp {
                container_index: cidx as u32,
                prop,
                value_type: value_kind.to_u8(),
                len: op.atom_len() as u32,
            })
        }
    }

    let cids = registers.cid_register.unwrap_vec();
    let container_arena = ContainerArena::from_containers(
        cids,
        &mut registers.peer_register,
        &mut registers.key_register,
    );

    // Write to output

    //      ┌────────────────────┬─────────────────────────────────────────┐
    //      │  Key Strings Size  │               Key Strings               │
    //      └────────────────────┴─────────────────────────────────────────┘
    let keys = registers.key_register.unwrap_vec();
    let keys_bytes = encode_keys(keys);

    //      ┌────────────────────┬─────────────────────────────────────────┐
    //      │  Position Size     │               Position                  │
    //      └────────────────────┴─────────────────────────────────────────┘
    let position_vec = registers.position_register.unwrap_vec();
    let positions = PositionArena::from_positions(position_vec.iter().map(|p| p.as_bytes()));
    let position_bytes = positions.encode_v2();

    //      ┌──────────┬──────────┬───────┬────────────────────────────────┐
    //      │          │          │       │                                │
    //      │          │          │       │                                │
    //      │          │          │       │                                │
    //      │  LEB128  │   RLE    │ Delta │                                │
    //      │ Lengths  │Containers│  RLE  │           ValueType            │
    //      │          │          │ Props │                                │
    //      │          │          │       │                                │
    //      │          │          │       │                                │
    //      │          │          │       │                                │
    //      └──────────┴──────────┴───────┴────────────────────────────────┘

    let ops_bytes = serde_columnar::to_vec(&EncodedOps { ops: encoded_ops }).unwrap();

    let delete_id_starts_bytes = if del_starts.is_empty() {
        Vec::new()
    } else {
        serde_columnar::to_vec(&EncodedDeleteStartIds {
            delete_start_ids: del_starts,
        })
        .unwrap()
    };
    //      ┌────────────────┬─────────────────────────────────────────────┐
    //      │Value Bytes Size│                Value Bytes                  │
    //      └────────────────┴─────────────────────────────────────────────┘

    // PeerIDs
    let mut peer_register = registers.peer_register;
    // .unwrap_vec();
    // let peer_bytes: Vec<u8> = peers.iter().flat_map(|p| p.to_le_bytes()).collect();

    // Change meta
    let (header, change_meta) = encode_changes(block, &mut peer_register);

    let value_bytes = value_writer.finish();
    let out = EncodedBlock {
        counter_start: block[0].id.counter as u32,
        counter_len: (block.last().unwrap().ctr_end() - block[0].id.counter) as u32,
        lamport_start: block[0].lamport(),
        lamport_len: block.last().unwrap().lamport_end() - block[0].lamport(),
        n_changes: block.len() as u32,
        header: header.into(),
        change_meta: change_meta.into(),
        cids: container_arena.encode().into(),
        keys: keys_bytes.into(),
        positions: position_bytes.into(),
        ops: ops_bytes.into(),
        delete_start_ids: delete_id_starts_bytes.into(),
        values: value_bytes.into(),
    };

    diagnose_block(&out);
    postcard::to_allocvec(&out).unwrap()
}

fn encode_keys(keys: Vec<loro_common::InternalString>) -> Vec<u8> {
    let mut keys_bytes = Vec::new();
    for key in keys {
        let bytes = key.as_bytes();
        leb128::write::unsigned(&mut keys_bytes, bytes.len() as u64).unwrap();
        keys_bytes.write_all(bytes).unwrap();
    }
    keys_bytes
}

fn decode_keys(mut bytes: &[u8]) -> LoroResult<Vec<InternalString>> {
    let mut keys = Vec::new();
    while !bytes.is_empty() {
        let len = leb128::read::unsigned(&mut bytes)
            .map_err(|_| LoroError::DecodeDataCorruptionError)? as usize;
        if bytes.len() < len {
            return Err(LoroError::DecodeDataCorruptionError);
        }
        let key =
            std::str::from_utf8(&bytes[..len]).map_err(|_| LoroError::DecodeDataCorruptionError)?;
        keys.push(key.into());
        bytes = &bytes[len..];
    }

    Ok(keys)
}

struct Registers {
    peer_register: ValueRegister<PeerID>,
    key_register: ValueRegister<loro_common::InternalString>,
    cid_register: ValueRegister<ContainerID>,
    position_register: ValueRegister<FractionalIndex>,
}

use crate::encoding::value::{
    RawTreeMove, Value, ValueDecodedArenasTrait, ValueEncodeRegister, ValueKind, ValueReader,
    ValueWriter,
};
use crate::oplog::change_store::block_meta_encode::encode_changes;
use crate::version::Frontiers;
impl ValueEncodeRegister for Registers {
    fn key_mut(&mut self) -> &mut ValueRegister<loro_common::InternalString> {
        &mut self.key_register
    }

    fn peer_mut(&mut self) -> &mut ValueRegister<PeerID> {
        &mut self.peer_register
    }

    fn encode_tree_op(&mut self, op: &tree_op::TreeOp) -> encoding::value::Value<'static> {
        match op {
            tree_op::TreeOp::Create {
                target,
                parent,
                position,
            } => encoding::value::Value::RawTreeMove(RawTreeMove {
                subject_peer_idx: self.peer_register.register(&target.peer),
                subject_cnt: target.counter,
                is_parent_null: parent.is_none(),
                parent_peer_idx: parent.map_or(0, |p| self.peer_register.register(&p.peer)),
                parent_cnt: parent.map_or(0, |p| p.counter),
                position_idx: self.position_register.register(position),
            }),
            tree_op::TreeOp::Move {
                target,
                parent,
                position,
            } => encoding::value::Value::RawTreeMove(RawTreeMove {
                subject_peer_idx: self.peer_register.register(&target.peer),
                subject_cnt: target.counter,
                is_parent_null: parent.is_none(),
                parent_peer_idx: parent.map_or(0, |p| self.peer_register.register(&p.peer)),
                parent_cnt: parent.map_or(0, |p| p.counter),
                position_idx: self.position_register.register(position),
            }),
            tree_op::TreeOp::Delete { target } => {
                let parent = TreeID::delete_root();
                encoding::value::Value::RawTreeMove(RawTreeMove {
                    subject_peer_idx: self.peer_register.register(&target.peer),
                    subject_cnt: target.counter,
                    is_parent_null: false,
                    parent_peer_idx: self.peer_register.register(&parent.peer),
                    parent_cnt: parent.counter,
                    position_idx: 0,
                })
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChangesBlockHeader {
    pub peer: PeerID,
    pub counter: Counter,
    pub n_changes: usize,
    pub peers: Vec<PeerID>,
    /// This has n + 1 elements, where counters[n] is the end counter of the
    /// last change in the block.
    ///
    /// You can infer the size of kth change by taking counters[k + 1] - counters[k]
    pub counters: Vec<Counter>,
    /// This has n elements
    pub lamports: Vec<Lamport>,
    pub deps_groups: Vec<Frontiers>,
    pub keys: OnceCell<Vec<InternalString>>,
    pub cids: OnceCell<Vec<ContainerID>>,
}

pub fn decode_header(m_bytes: &[u8]) -> LoroResult<ChangesBlockHeader> {
    let doc = postcard::from_bytes(m_bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;

    decode_header_from_doc(&doc)
}

// MARK: decode_block_from_doc
fn decode_header_from_doc(doc: &EncodedBlock) -> Result<ChangesBlockHeader, LoroError> {
    let EncodedBlock {
        n_changes,
        header,
        counter_len,
        counter_start,
        lamport_len,
        lamport_start,
        ..
    } = doc;
    let ans: ChangesBlockHeader = decode_changes_header(
        header,
        *n_changes as usize,
        *counter_start as Counter,
        *counter_len as Counter,
        *lamport_start,
        *lamport_len,
    )?;
    Ok(ans)
}

#[columnar(vec, ser, de, iterable)]
#[derive(Debug, Clone)]
struct EncodedOp {
    #[columnar(strategy = "DeltaRle")]
    container_index: u32,
    #[columnar(strategy = "DeltaRle")]
    prop: i32,
    #[columnar(strategy = "Rle")]
    value_type: u8,
    #[columnar(strategy = "Rle")]
    len: u32,
}

#[columnar(ser, de)]
struct EncodedOps {
    #[columnar(class = "vec", iter = "EncodedOp")]
    ops: Vec<EncodedOp>,
}

#[columnar(ser, de)]
struct EncodedDeleteStartIds {
    #[columnar(class = "vec", iter = "EncodedDeleteStartId")]
    delete_start_ids: Vec<EncodedDeleteStartId>,
}

struct ValueDecodeArena<'a> {
    peers: &'a [PeerID],
    keys: &'a [InternalString],
}

impl ValueDecodedArenasTrait for ValueDecodeArena<'_> {
    fn keys(&self) -> &[InternalString] {
        self.keys
    }

    fn peers(&self) -> &[PeerID] {
        self.peers
    }

    fn decode_tree_op(
        &self,
        _positions: &[Vec<u8>],
        _op: encoding::value::EncodedTreeMove,
        _id: ID,
    ) -> LoroResult<tree_op::TreeOp> {
        Err(LoroError::DecodeDataCorruptionError)
    }
}

pub fn decode_block_range(
    mut bytes: &[u8],
) -> LoroResult<((Counter, Counter), (Lamport, Lamport))> {
    let version = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read version: {e}").into_boxed_str())
    })?;

    if version as u16 != VERSION {
        return Err(LoroError::DecodeError(
            "Version mismatch".to_string().into_boxed_str(),
        ));
    }

    let counter_start = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read counter start: {e}").into_boxed_str())
    })? as Counter;
    let counter_len = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read counter length: {e}").into_boxed_str())
    })? as Counter;
    let lamport_start = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read lamport start: {e}").into_boxed_str())
    })? as Lamport;
    let lamport_len = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read lamport length: {e}").into_boxed_str())
    })? as Lamport;
    Ok((
        (counter_start, counter_start + counter_len),
        (lamport_start, lamport_start + lamport_len),
    ))
}

/// Ensure the cids in header are decoded
#[allow(unused)]
pub fn decode_cids(
    bytes: &[u8],
    header: Option<ChangesBlockHeader>,
) -> LoroResult<ChangesBlockHeader> {
    let doc = postcard::from_bytes(bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;
    let header = if let Some(h) = header {
        h
    } else {
        let doc = postcard::from_bytes(bytes).map_err(|e| {
            LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str())
        })?;
        decode_header_from_doc(&doc)?
    };

    if header.cids.get().is_some() {
        return Ok(header);
    }

    let EncodedBlock { cids, keys, .. } = doc;
    let keys = header.keys.get_or_try_init(|| decode_keys(&keys))?;
    let decode_arena = ValueDecodeArena {
        peers: &header.peers,
        keys,
    };

    header.cids.get_or_try_init(|| {
        ContainerArena::decode(&cids)?
            .iter()
            .map(|x| x.as_container_id(&decode_arena))
            .try_collect()
    })?;
    Ok(header)
}

// MARK: decode_block
pub fn decode_block(
    m_bytes: &[u8],
    shared_arena: &SharedArena,
    header: Option<&ChangesBlockHeader>,
) -> LoroResult<Vec<Change>> {
    decode_block_with_budget(m_bytes, shared_arena, header, None)
}

/// [`decode_block`], with the block's counts checked against `budget` when
/// there is one.
///
/// Under a budget, the block's change count and length in atoms are taken
/// from the budget before anything is decoded. The columns that run length
/// encoding can expand are decoded lazily: ops and positions are refused
/// once there are more of them than the block has atoms (an honest block
/// has at most one op per atom and one position per op), and delete starts
/// are decoded one at a time as delete ops use them. Decoded fractional
/// indexes count against `decompressed_bytes`.
pub(crate) fn decode_block_with_budget(
    m_bytes: &[u8],
    shared_arena: &SharedArena,
    header: Option<&ChangesBlockHeader>,
    mut budget: Option<&mut ImportBudget>,
) -> LoroResult<Vec<Change>> {
    let doc: EncodedBlock = postcard::from_bytes(m_bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;
    let max_column_len = doc.counter_len as usize;
    if let Some(budget) = budget.as_deref_mut() {
        budget.charge_block(doc.n_changes as usize, max_column_len)?;
    }
    let header_on_stack;
    let header = if let Some(header) = header {
        header
    } else {
        header_on_stack = decode_header_from_doc(&doc)?;
        &header_on_stack
    };
    let EncodedBlock {
        n_changes,
        counter_start: first_counter,
        change_meta,
        cids,
        keys,
        ops,
        delete_start_ids,
        values,
        positions,
        ..
    } = doc;
    let n_changes = n_changes as usize;
    // The header was decoded from these bytes, which bounds its change count
    // by the header's length; the allocations below rely on that.
    if n_changes != header.n_changes {
        return Err(LoroError::DecodeDataCorruptionError);
    }
    let mut changes = Vec::with_capacity(n_changes);
    let timestamp_decoder = DeltaOfDeltaDecoder::<i64>::new(&change_meta)
        .map_err(|_| LoroError::DecodeDataCorruptionError)?;
    let (timestamps, bytes) = timestamp_decoder
        .take_n_finalize(n_changes)
        .map_err(|_| LoroError::DecodeDataCorruptionError)?;
    let commit_msg_len_decoder = AnyRleDecoder::<u32>::new(bytes);
    let (commit_msg_lens, commit_msgs) = commit_msg_len_decoder
        .take_n_finalize(n_changes)
        .map_err(|_| LoroError::DecodeDataCorruptionError)?;
    let mut commit_msg_index = 0u32;
    let keys = header.keys.get_or_try_init(|| decode_keys(&keys))?;
    let decode_arena = ValueDecodeArena {
        peers: &header.peers,
        keys,
    };
    let positions = match budget.as_deref_mut() {
        None => PositionArena::decode_v2(&positions)?.try_parse_to_positions()?,
        Some(budget) => PositionArena::decode_v2_capped(&positions, max_column_len)?
            .try_parse_to_positions_with_budget(budget)?,
    };
    // The container arena is a plain sequence of rows (not run length
    // encoded), so its length is bounded by its bytes.
    let cids: &Vec<ContainerID> = header.cids.get_or_try_init(|| {
        ContainerArena::decode(&cids)?
            .iter()
            .map(|x| x.as_container_id(&decode_arena))
            .try_collect()
    })?;
    let mut value_reader = ValueReader::new(&values);
    let encoded_ops_iters = serde_columnar::iter_from_bytes::<EncodedOps>(&ops)?;
    let op_iter = encoded_ops_iters.ops;
    let mut del_iter: Box<dyn Iterator<Item = Result<EncodedDeleteStartId, ColumnarError>>> =
        if delete_start_ids.is_empty() {
            Box::new(std::iter::empty())
        } else if budget.is_some() {
            // Decoded one by one as delete ops need them, so a run length
            // encoded column never expands past the ops of the block.
            Box::new(
                serde_columnar::iter_from_bytes::<EncodedDeleteStartIds>(&delete_start_ids)?
                    .delete_start_ids,
            )
        } else {
            let decoded: EncodedDeleteStartIds = serde_columnar::from_bytes(&delete_start_ids)?;
            Box::new(decoded.delete_start_ids.into_iter().map(Ok))
        };
    for i in 0..n_changes {
        let commit_msg: Option<Arc<str>> = {
            let len = *commit_msg_lens
                .get(i)
                .ok_or(LoroError::DecodeDataCorruptionError)?;
            if len == 0 {
                None
            } else {
                let end = commit_msg_index
                    .checked_add(len)
                    .ok_or(LoroError::DecodeDataCorruptionError)?;
                if commit_msgs.len() < end as usize {
                    return Err(LoroError::DecodeDataCorruptionError);
                }
                match std::str::from_utf8(&commit_msgs[commit_msg_index as usize..end as usize]) {
                    Ok(s) => {
                        commit_msg_index = end;
                        Some(Arc::from(s))
                    }
                    Err(_) => {
                        tracing::error!("Invalid UTF8 String");
                        return LoroResult::Err(LoroError::DecodeDataCorruptionError);
                    }
                }
            }
        };
        changes.push(Change {
            ops: Default::default(),
            deps: header
                .deps_groups
                .get(i)
                .ok_or(LoroError::DecodeDataCorruptionError)?
                .clone(),
            id: ID::new(
                header.peer,
                *header
                    .counters
                    .get(i)
                    .ok_or(LoroError::DecodeDataCorruptionError)?,
            ),
            lamport: *header
                .lamports
                .get(i)
                .ok_or(LoroError::DecodeDataCorruptionError)?,
            timestamp: *timestamps
                .get(i)
                .ok_or(LoroError::DecodeDataCorruptionError)? as Timestamp,
            commit_msg,
        })
    }

    let mut counter = first_counter as Counter;
    let mut change_index = 0;
    let peer = header.peer;
    let mut n_ops = 0usize;
    for op in op_iter {
        let EncodedOp {
            container_index,
            prop,
            value_type,
            len,
        } = op?;
        if budget.is_some() {
            n_ops += 1;
            if n_ops > max_column_len {
                return Err(LoroError::DecodeError(
                    "Decode block error: more ops than atoms".into(),
                ));
            }
        }
        let value = Value::decode(
            ValueKind::from_u8(value_type),
            &mut value_reader,
            &decode_arena,
            ID::new(peer, counter),
        )?;

        let cid = cids
            .get(container_index as usize)
            .ok_or(LoroError::DecodeDataCorruptionError)?;
        let content = decode_op(
            cid,
            value,
            &mut del_iter,
            shared_arena,
            &decode_arena,
            &positions,
            prop,
            ID::new(peer, counter),
        )?;
        if let (Some(budget), InnerContent::Tree(tree_op)) = (budget.as_deref_mut(), &content) {
            if let tree_op::TreeOp::Create { position, .. }
            | tree_op::TreeOp::Move { position, .. } = tree_op.as_ref()
            {
                budget.charge_decompressed(position.as_bytes().len())?;
            }
        }

        let c_idx = shared_arena.register_container(cid);
        let op = Op {
            counter,
            container: c_idx,
            content,
        };

        changes
            .get_mut(change_index)
            .ok_or(LoroError::DecodeDataCorruptionError)?
            .ops
            .push(op);
        counter = counter
            .checked_add(len as Counter)
            .ok_or(LoroError::DecodeDataCorruptionError)?;
        let next_counter = header
            .counters
            .get(change_index + 1)
            .ok_or(LoroError::DecodeDataCorruptionError)?;
        if counter >= *next_counter {
            change_index += 1;
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod test {
    use std::{borrow::Cow, panic::AssertUnwindSafe};

    use loro_common::ContainerType;

    use crate::encoding::value::{EncodedTreeMove, Value};
    use crate::encoding::IMPORT_BUDGET_EXCEEDED;
    use crate::{cursor::PosType, delta::DeltaValue, loro::ExportMode, LoroDoc};
    use serde_columnar::{AnyRleEncoder, BoolRleEncoder};

    use super::*;

    /// The bytes of the one change block of `doc`, a document of one peer.
    fn only_block(doc: &LoroDoc) -> Vec<u8> {
        let oplog = doc.oplog.lock();
        let mut changes = Vec::new();
        oplog
            .change_store()
            .visit_all_changes(&mut |change| changes.push(change.clone()));
        encode_block(&changes, &oplog.arena)
    }

    /// A block of one change with two map sets.
    fn map_block() -> Vec<u8> {
        let doc = LoroDoc::new_auto_commit();
        doc.set_peer_id(1).unwrap();
        doc.get_map("map").insert("x", 1).unwrap();
        doc.get_map("map").insert("y", 2).unwrap();
        doc.commit_then_renew();
        only_block(&doc)
    }

    /// `block` with `f` applied to its fields.
    fn craft(block: &[u8], f: impl FnOnce(&mut EncodedBlock<'_>)) -> Vec<u8> {
        let mut encoded: EncodedBlock = postcard::from_bytes(block).unwrap();
        f(&mut encoded);
        postcard::to_allocvec(&encoded).unwrap()
    }

    /// A fast update blob with `block` as its only block.
    fn update_blob(block: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        leb128::write::unsigned(&mut body, block.len() as u64).unwrap();
        body.extend_from_slice(block);
        crate::encoding::encode_blob_for_test(crate::encoding::EncodeMode::FastUpdates, &body)
    }

    fn small_budget() -> ImportBudget {
        ImportBudget {
            changes: 100,
            ops: 100,
            decompressed_bytes: 1 << 20,
        }
    }

    /// `table`, a columnar table of one row whose columns are all run length
    /// encoded, with that row repeated `n` times: each column's run count
    /// becomes `n`. A few bytes that decode to `n` rows.
    fn repeat_rows(table: &[u8], n: u64) -> Vec<u8> {
        let mut r = table;
        let mut ans = Vec::new();
        // The field count of the table struct and the column count.
        let fields = leb128::read::unsigned(&mut r).unwrap();
        assert_eq!(fields, 1);
        let columns = leb128::read::unsigned(&mut r).unwrap();
        leb128::write::unsigned(&mut ans, fields).unwrap();
        leb128::write::unsigned(&mut ans, columns).unwrap();
        for _ in 0..columns {
            let len = leb128::read::unsigned(&mut r).unwrap() as usize;
            let (mut column, rest) = r.split_at(len);
            r = rest;
            // One row: a zigzag encoded count of 1 (a run) or -1 (a literal
            // run), then the value. A run of n repeats the same value.
            let count = leb128::read::unsigned(&mut column).unwrap();
            assert!(count == 2 || count == 1, "one row, not {count}");
            let mut new_column = Vec::new();
            leb128::write::unsigned(&mut new_column, n * 2).unwrap();
            new_column.extend_from_slice(column);
            leb128::write::unsigned(&mut ans, new_column.len() as u64).unwrap();
            ans.extend_from_slice(&new_column);
        }
        assert!(r.is_empty());
        ans
    }

    #[test]
    fn crafted_blocks_import_when_unchanged() {
        let block = map_block();
        let doc = LoroDoc::new();
        doc.import_with_budget(&update_blob(&craft(&block, |_| {})), small_budget())
            .unwrap();
        assert_eq!(doc.oplog_vv().get(&1), Some(&2));
    }

    #[test]
    fn a_huge_change_count_is_refused_before_it_sizes_allocations() {
        let crafted = craft(&map_block(), |b| b.n_changes = u32::MAX);
        let arena = SharedArena::new();
        assert!(decode_block(&crafted, &arena, None).is_err());
        assert!(decode_header(&crafted).is_err());
        let blob = update_blob(&crafted);
        let err = LoroDoc::new()
            .import_with_budget(&blob, small_budget())
            .unwrap_err();
        assert!(err.to_string().contains(IMPORT_BUDGET_EXCEEDED), "{err}");
        assert!(LoroDoc::new().import(&blob).is_err());
    }

    #[test]
    fn a_huge_dep_count_is_refused_before_it_sizes_allocations() {
        let crafted = craft(&map_block(), |b| {
            let mut header = Vec::new();
            leb128::write::unsigned(&mut header, 1).unwrap();
            header.extend_from_slice(&1u64.to_le_bytes());
            let mut dep_self = BoolRleEncoder::new();
            dep_self.append(false).unwrap();
            header.extend(dep_self.finish().unwrap());
            let mut dep_len = AnyRleEncoder::<usize>::new();
            dep_len.append(1 << 40).unwrap();
            header.extend(dep_len.finish().unwrap());
            header.extend_from_slice(&[0; 16]);
            b.header = Cow::Owned(header);
        });
        let arena = SharedArena::new();
        assert!(decode_header(&crafted).is_err());
        assert!(decode_block(&crafted, &arena, None).is_err());
        let blob = update_blob(&crafted);
        assert!(LoroDoc::new()
            .import_with_budget(&blob, small_budget())
            .is_err());
        assert!(LoroDoc::new().import(&blob).is_err());
    }

    /// An ops column that run length encoding expands to 10^8 ops of length
    /// 0 is refused under a budget after one op more than the block's atoms.
    #[test]
    fn ops_past_the_atoms_are_refused_under_a_budget() {
        let one = serde_columnar::to_vec(&EncodedOps {
            ops: vec![EncodedOp {
                container_index: 0,
                prop: 0,
                value_type: 8, // DeleteOnce, a map delete
                len: 0,
            }],
        })
        .unwrap();
        let ops = repeat_rows(&one, 100_000_000);
        let iter = serde_columnar::iter_from_bytes::<EncodedOps>(&ops).unwrap();
        assert_eq!(iter.ops.take(1000).count(), 1000, "the column expands");
        let crafted = craft(&map_block(), |b| b.ops = Cow::Owned(ops));
        let arena = SharedArena::new();
        let mut budget = small_budget();
        let err = decode_block_with_budget(&crafted, &arena, None, Some(&mut budget)).unwrap_err();
        assert!(err.to_string().contains("more ops than atoms"), "{err}");
        assert!(LoroDoc::new()
            .import_with_budget(&update_blob(&crafted), small_budget())
            .is_err());
    }

    /// A delete start column that expands to 10^8 entries is read one entry
    /// per delete op under a budget, so the block decodes as before.
    #[test]
    fn delete_starts_are_decoded_lazily_under_a_budget() {
        let doc = LoroDoc::new_auto_commit();
        doc.set_peer_id(1).unwrap();
        let text = doc.get_text("text");
        text.insert(0, "hello", crate::cursor::PosType::Unicode)
            .unwrap();
        text.delete(1, 2, crate::cursor::PosType::Unicode).unwrap();
        doc.commit_then_renew();
        let block = only_block(&doc);
        let encoded: EncodedBlock = postcard::from_bytes(&block).unwrap();
        let starts = repeat_rows(&encoded.delete_start_ids, 100_000_000);
        let crafted = craft(&block, |b| b.delete_start_ids = Cow::Owned(starts));
        let target = LoroDoc::new();
        target
            .import_with_budget(&update_blob(&crafted), small_budget())
            .unwrap();
        assert_eq!(target.get_deep_value(), doc.get_deep_value());
    }

    #[test]
    fn decode_block_rejects_corrupt_payload_without_panic() {
        let doc = LoroDoc::new_auto_commit();
        doc.get_map("map").insert("x", 100).unwrap();
        doc.commit_then_renew();

        let oplog = doc.oplog.lock();
        let mut changes = Vec::new();
        oplog
            .change_store()
            .visit_all_changes(&mut |change| changes.push(change.clone()));
        let block_bytes = encode_block(&changes, &oplog.arena);
        let mut encoded: EncodedBlock = postcard::from_bytes(&block_bytes).unwrap();
        encoded.change_meta = Cow::Owned(vec![0x80]);
        let corrupt = postcard::to_allocvec(&encoded).unwrap();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            decode_block(&corrupt, &oplog.arena, None)
        }));
        assert!(result.is_ok(), "corrupt block payload should not panic");
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn tree_move_payload_is_rejected_without_panic() {
        let arena = SharedArena::new();
        let value_arena = ValueDecodeArena {
            peers: &[1],
            keys: &[],
        };
        let cid = ContainerID::Root {
            name: "tree".into(),
            container_type: ContainerType::Tree,
        };
        let value = Value::TreeMove(EncodedTreeMove {
            target_idx: 0,
            is_parent_null: true,
            parent_idx: 0,
            position: 0,
        });
        let mut deletes = std::iter::empty();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            decode_op(
                &cid,
                value,
                &mut deletes,
                &arena,
                &value_arena,
                &[],
                0,
                ID::new(1, 0),
            )
        }));

        assert!(result.is_ok(), "tree move payload should not panic");
        assert!(matches!(
            result.unwrap(),
            Err(LoroError::DecodeDataCorruptionError)
        ));
    }

    #[test]
    pub fn encode_single_text_edit() {
        let doc = LoroDoc::new();
        doc.start_auto_commit();
        doc.get_map("map").insert("x", 100).unwrap();
        // doc.get_text("text").insert(0, "Hi").unwrap();
        // let node = doc.get_tree("tree").create(None).unwrap();
        // doc.get_tree("tree").create(node).unwrap();
        diagnose(&doc);
        doc.get_map("map").insert("y", 20).unwrap();
        diagnose(&doc);
        doc.get_map("map").insert("z", 1000).unwrap();
        diagnose(&doc);
        doc.get_text("text")
            .insert(0, "Hello", PosType::Unicode)
            .unwrap();
        diagnose(&doc);
        doc.get_text("text")
            .insert(2, "He", PosType::Unicode)
            .unwrap();
        diagnose(&doc);
        doc.get_text("text").delete(1, 4, PosType::Unicode).unwrap();
        diagnose(&doc);
        doc.get_text("text").delete(0, 2, PosType::Unicode).unwrap();
        diagnose(&doc);
    }

    fn diagnose(doc: &LoroDoc) {
        let bytes = doc.export(ExportMode::all_updates()).unwrap();
        println!("Old Update bytes {:?}", dev_utils::ByteSize(bytes.length()));

        let bytes = doc.export(ExportMode::all_updates()).unwrap();
        println!("Update bytes {:?}", dev_utils::ByteSize(bytes.length()));
        // assert!(bytes.len() < 30);

        let bytes = doc.export(ExportMode::Snapshot).unwrap();
        println!("Snapshot bytes {:?}", dev_utils::ByteSize(bytes.length()));
        // assert!(bytes.len() < 30);

        let json = doc.export_json_updates(&Default::default(), &doc.oplog_vv(), true);
        let json_string = serde_json::to_string(&json.changes).unwrap();
        println!(
            "JSON string bytes {:?}",
            dev_utils::ByteSize(json_string.len())
        );
        let bytes = postcard::to_allocvec(&json.changes).unwrap();
        println!("JSON update bytes {:?}", dev_utils::ByteSize(bytes.len()));
        println!("\n\n")
    }
}
