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
use serde_columnar::{columnar, AnyRleDecoder, DeltaOfDeltaDecoder, Itertools};
use tracing::info;

use super::block_meta_encode::{check_delta_of_delta, decode_changes_header};
use crate::arena::SharedArena;
use crate::change::{Change, Timestamp};
use crate::container::tree::tree_op;
use crate::encoding::arena::{ContainerArena, PositionArena};
use crate::encoding::value_register::ValueRegister;
use crate::encoding::{
    self, decode_op, encode_op, get_op_prop, EncodedDeleteStartId, IterableEncodedDeleteStartId,
};
use crate::op::Op;

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
    let overflow = || LoroError::DecodeError("Decode block error: range overflow".into());
    Ok((
        (
            counter_start,
            counter_start
                .checked_add(counter_len)
                .ok_or_else(overflow)?,
        ),
        (
            lamport_start,
            lamport_start
                .checked_add(lamport_len)
                .ok_or_else(overflow)?,
        ),
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
    let doc = postcard::from_bytes(m_bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;
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
    let mut changes = Vec::with_capacity(n_changes);
    check_delta_of_delta(&change_meta, n_changes)?;
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
    let positions = PositionArena::decode_v2(&positions)?;
    let positions = positions.try_parse_to_positions()?;
    let cids: &Vec<ContainerID> = header.cids.get_or_try_init(|| {
        ContainerArena::decode(&cids)?
            .iter()
            .map(|x| x.as_container_id(&decode_arena))
            .try_collect()
    })?;
    let mut value_reader = ValueReader::new(&values);
    let encoded_ops_iters = serde_columnar::iter_from_bytes::<EncodedOps>(&ops)?;
    let op_iter = encoded_ops_iters.ops;
    let encoded_delete_id_starts: EncodedDeleteStartIds = if delete_start_ids.is_empty() {
        EncodedDeleteStartIds {
            delete_start_ids: Vec::new(),
        }
    } else {
        serde_columnar::from_bytes(&delete_start_ids)?
    };
    let mut del_iter = encoded_delete_id_starts
        .delete_start_ids
        .into_iter()
        .map(Ok);
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
    for op in op_iter {
        let EncodedOp {
            container_index,
            prop,
            value_type,
            len,
        } = op?;
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
    use crate::{cursor::PosType, delta::DeltaValue, loro::ExportMode, LoroDoc};
    use loro_common::LoroValue;
    use serde_columnar::{AnyRleEncoder, BoolRleEncoder, DeltaOfDeltaEncoder};

    use super::*;

    // Crafted blocks that used to panic during decode. Each is wrapped in a
    // fast-update blob and imported; the import must fail and leave the
    // document usable.

    const PEER: PeerID = 7;

    /// A document of `PEER` with one change per entry of `msgs`; each change
    /// is made by `edit` and carries its own commit message, so changes do
    /// not merge.
    fn doc_with_changes(msgs: &[&str], edit: impl Fn(&LoroDoc, usize)) -> LoroDoc {
        let doc = LoroDoc::new_auto_commit();
        doc.set_peer_id(PEER).unwrap();
        for (i, msg) in msgs.iter().enumerate() {
            doc.set_next_commit_message(msg);
            edit(&doc, i);
            doc.commit_then_renew();
        }
        doc
    }

    /// The only block of `doc`'s full update.
    fn only_block(doc: &LoroDoc) -> Vec<u8> {
        let bytes = doc.export(ExportMode::all_updates()).unwrap();
        let mut body = &bytes[22..];
        let len = leb128::read::unsigned(&mut body).unwrap() as usize;
        assert_eq!(body.len(), len, "the update has one block");
        body.to_vec()
    }

    /// A fast-update blob that holds `block`.
    fn update_with_block(block: &[u8]) -> Vec<u8> {
        let mut ans = b"loro".to_vec();
        ans.extend([0; 16]);
        ans.extend(crate::encoding::EncodeMode::FastUpdates.to_bytes());
        leb128::write::unsigned(&mut ans, block.len() as u64).unwrap();
        ans.extend_from_slice(block);
        let checksum = xxhash_rust::xxh32::xxh32(&ans[20..], u32::from_le_bytes(*b"LORO"));
        ans[16..20].copy_from_slice(&checksum.to_le_bytes());
        ans
    }

    /// `doc`'s block rewritten by `f`, as a fast-update blob.
    fn crafted_update(doc: &LoroDoc, f: impl FnOnce(&mut EncodedBlock)) -> Vec<u8> {
        let block = only_block(doc);
        let mut encoded: EncodedBlock = postcard::from_bytes(&block).unwrap();
        f(&mut encoded);
        update_with_block(&postcard::to_allocvec(&encoded).unwrap())
    }

    /// Imports `update` into `doc`, which must fail, and checks that the
    /// document still takes local edits.
    fn assert_import_fails(doc: &LoroDoc, update: &[u8]) {
        assert!(doc.import(update).is_err());
        doc.get_map("after").insert("still", "usable").unwrap();
        doc.commit_then_renew();
        assert_eq!(
            doc.get_map("after").get("still"),
            Some(LoroValue::from("usable"))
        );
    }

    fn assert_import_fails_on_new_doc(update: &[u8]) {
        let doc = LoroDoc::new_auto_commit();
        assert_import_fails(&doc, update);
    }

    /// Big-endian bits, as `DeltaOfDeltaEncoder` writes them.
    fn bits(bits: &str) -> Vec<u8> {
        let bits: Vec<u8> = bits
            .bytes()
            .filter(|b| *b != b' ')
            .map(|b| b - b'0')
            .collect();
        bits.chunks(8)
            .map(|c| {
                c.iter()
                    .enumerate()
                    .fold(0u8, |acc, (i, b)| acc | (b << (7 - i)))
            })
            .collect()
    }

    /// A delta-of-delta stream with `head`, `last_used_bit` and `body` bits.
    fn delta_of_delta(head: Option<i64>, last_used_bit: u8, body: &[u8]) -> Vec<u8> {
        let mut ans = postcard::to_allocvec(&head).unwrap();
        ans.push(last_used_bit);
        ans.extend_from_slice(body);
        ans
    }

    fn two_map_changes() -> LoroDoc {
        doc_with_changes(&["a", "b"], |doc, i| {
            doc.get_map("m").insert(&i.to_string(), i as i64).unwrap();
        })
    }

    /// Timestamps whose delta-of-delta stream claims more bits than it has
    /// made the decoder index past its buffer.
    #[test]
    fn timestamps_with_an_overlong_bit_count_are_an_import_error() {
        let doc = two_map_changes();
        let update = crafted_update(&doc, |b| {
            b.change_meta = Cow::Owned(delta_of_delta(Some(0), 0xff, &bits("1000 0000")));
        });
        assert_import_fails_on_new_doc(&update);
    }

    /// A flag bit at the very end of the stream made the decoder unwrap a
    /// missing read.
    #[test]
    fn timestamps_ending_in_a_flag_bit_are_an_import_error() {
        let doc = two_map_changes();
        let update = crafted_update(&doc, |b| {
            b.change_meta = Cow::Owned(delta_of_delta(Some(0), 1, &bits("1000 0000")));
        });
        assert_import_fails_on_new_doc(&update);
    }

    /// Deltas that overflow `i64` panicked in debug builds.
    #[test]
    fn timestamps_that_overflow_are_an_import_error() {
        let doc = two_map_changes();
        // Flags 1, 1, 1, 1, 1: a 64-bit delta of delta, here i64::MAX.
        let stream = format!("11111 0{}", "1".repeat(63));
        let mut body = bits(&stream);
        let used = (stream.replace(' ', "").len() % 8) as u8;
        let last_used_bit = if used == 0 { 8 } else { used };
        if used == 0 {
            body.push(0);
        }
        let update = crafted_update(&doc, |b| {
            b.change_meta = Cow::Owned(delta_of_delta(Some(1), last_used_bit, &body));
        });
        assert_import_fails_on_new_doc(&update);
    }

    /// `check_delta_of_delta` accepts what the encoder writes and agrees
    /// with the decoder on it.
    #[test]
    fn delta_of_delta_check_accepts_encoded_streams() {
        let cases: Vec<Vec<i64>> = vec![
            vec![],
            vec![5],
            vec![0, 1, 2, 3],
            vec![1_700_000_000_000, 1_700_000_000_007, 1_700_000_100_000, 3],
            vec![i64::MIN / 4, i64::MAX / 4, 0, -1, 1 << 40],
            (0..1000).map(|i| i * i * 37 % 1001).collect(),
        ];
        for values in cases {
            let mut encoder = DeltaOfDeltaEncoder::new();
            for v in &values {
                encoder.append(*v).unwrap();
            }
            let bytes = encoder.finish().unwrap();
            check_delta_of_delta(&bytes, values.len()).unwrap();
            let (decoded, rest) = DeltaOfDeltaDecoder::<i64>::new(&bytes)
                .unwrap()
                .take_n_finalize(values.len())
                .unwrap();
            assert_eq!(decoded, values);
            assert!(rest.is_empty());
        }
    }

    /// Builds a block header the way `encode_changes` does, from its parts.
    fn header(
        lengths: &[u64],
        dep_self: &[bool],
        deps_len: &[usize],
        lamports: &[i64],
        trailing: &[u8],
    ) -> Vec<u8> {
        let mut ans = Vec::new();
        leb128::write::unsigned(&mut ans, 1).unwrap();
        ans.extend(PEER.to_le_bytes());
        for len in lengths {
            leb128::write::unsigned(&mut ans, *len).unwrap();
        }
        let mut dep_self_encoder = BoolRleEncoder::new();
        for b in dep_self {
            dep_self_encoder.append(*b).unwrap();
        }
        ans.extend(dep_self_encoder.finish().unwrap());
        let mut deps_len_encoder = AnyRleEncoder::<usize>::new();
        for l in deps_len {
            deps_len_encoder.append(*l).unwrap();
        }
        ans.extend(deps_len_encoder.finish().unwrap());
        ans.extend(AnyRleEncoder::<u32>::new().finish().unwrap());
        ans.extend(DeltaOfDeltaEncoder::new().finish().unwrap());
        let mut lamport_encoder = DeltaOfDeltaEncoder::new();
        for l in lamports {
            lamport_encoder.append(*l).unwrap();
        }
        ans.extend(lamport_encoder.finish().unwrap());
        ans.extend_from_slice(trailing);
        ans
    }

    /// The honest header of `two_map_changes` decodes like the original, so
    /// the crafted headers below differ only in what each test changes.
    #[test]
    fn crafted_header_matches_the_encoder() {
        let doc = two_map_changes();
        let block = only_block(&doc);
        let encoded: EncodedBlock = postcard::from_bytes(&block).unwrap();
        assert_eq!(
            encoded.header.as_ref(),
            header(&[1], &[false, true], &[0, 0], &[0], &[]).as_slice()
        );
    }

    /// Dep counts whose sum overflows `usize` panicked in debug builds.
    #[test]
    fn dep_counts_that_overflow_are_an_import_error() {
        let doc = two_map_changes();
        let update = crafted_update(&doc, |b| {
            b.header = Cow::Owned(header(&[1], &[false, true], &[usize::MAX, 1], &[0], &[]));
        });
        assert_import_fails_on_new_doc(&update);
    }

    /// Trailing bytes after the header failed a debug assert.
    #[test]
    fn trailing_header_bytes_are_an_import_error() {
        let doc = two_map_changes();
        let update = crafted_update(&doc, |b| {
            b.header = Cow::Owned(header(&[1], &[false, true], &[0, 0], &[0], &[0, 0]));
        });
        assert_import_fails_on_new_doc(&update);
    }

    /// A mark whose end overflowed `u32` panicked in debug builds.
    #[test]
    fn a_mark_end_that_overflows_is_an_import_error() {
        let doc = doc_with_changes(&["a"], |doc, _| {
            let text = doc.get_text("t");
            text.insert(0, "ab", PosType::Unicode).unwrap();
            text.mark(0, 2, "bold", true.into(), PosType::Unicode)
                .unwrap();
        });
        let update = crafted_update(&doc, |b| {
            let mut ops: EncodedOps = serde_columnar::from_bytes(&b.ops).unwrap();
            let mark = ops
                .ops
                .iter_mut()
                .find(|op| matches!(ValueKind::from_u8(op.value_type), ValueKind::MarkStart))
                .unwrap();
            mark.prop = -1;
            b.ops = Cow::Owned(serde_columnar::to_vec(&ops).unwrap());
        });
        assert_import_fails_on_new_doc(&update);
    }

    /// `decode_block_range` reads the ranges of lazily loaded blocks; ends
    /// that overflow are errors.
    #[test]
    fn block_ranges_that_overflow_are_errors() {
        let mut bytes = Vec::new();
        for v in [0, i32::MAX as u64, 1, 0, 1] {
            leb128::write::unsigned(&mut bytes, v).unwrap();
        }
        assert!(decode_block_range(&bytes).is_err());
        let mut bytes = Vec::new();
        for v in [0, 0, 1, u32::MAX as u64, 1] {
            leb128::write::unsigned(&mut bytes, v).unwrap();
        }
        assert!(decode_block_range(&bytes).is_err());
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
