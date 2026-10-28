use loro_common::{Counter, Lamport, LoroError, LoroResult, PeerID, ID};
use once_cell::sync::OnceCell;
use rle::HasLength;
use serde_columnar::{
    AnyRleDecoder, AnyRleEncoder, BoolRleDecoder, BoolRleEncoder, DeltaOfDeltaDecoder,
    DeltaOfDeltaEncoder,
};

use crate::{change::Change, encoding::value_register::ValueRegister, version::Frontiers};

use super::block_encode::ChangesBlockHeader;

pub(crate) fn encode_changes(
    block: &[Change],
    peer_register: &mut ValueRegister<PeerID>,
) -> (Vec<u8>, Vec<u8>) {
    let peer = block[0].peer();
    let mut timestamp_encoder = DeltaOfDeltaEncoder::new();
    let mut lamport_encoder = DeltaOfDeltaEncoder::new();
    let mut commit_msg_len_encoder = AnyRleEncoder::<u32>::new();
    let mut commit_msgs = String::new();
    let mut dep_self_encoder = BoolRleEncoder::new();
    let mut dep_len_encoder = AnyRleEncoder::<usize>::new();
    let mut encoded_deps = EncodedDeps {
        peer_idx: AnyRleEncoder::new(),
        counter: DeltaOfDeltaEncoder::new(),
    };
    // First Counter + Change Len
    let mut lengths_bytes = Vec::new();
    let mut counter = vec![];

    for (i, c) in block.iter().enumerate() {
        counter.push(c.id.counter);
        let is_last = i == block.len() - 1;
        if !is_last {
            leb128::write::unsigned(&mut lengths_bytes, c.atom_len() as u64).unwrap();
            lamport_encoder.append(c.lamport() as i64).unwrap();
        }
        timestamp_encoder.append(c.timestamp()).unwrap();
        if let Some(msg) = c.commit_msg.as_ref() {
            commit_msg_len_encoder.append(msg.len() as u32).unwrap();
            commit_msgs.push_str(msg);
        } else {
            commit_msg_len_encoder.append(0).unwrap();
        }

        let mut dep_on_self = false;
        for dep in c.deps().iter() {
            if dep.peer == peer {
                dep_on_self = true;
            } else {
                let peer_idx = peer_register.register(&dep.peer);
                encoded_deps.peer_idx.append(peer_idx as u32).unwrap();
                encoded_deps.counter.append(dep.counter as i64).unwrap();
            }
        }

        dep_self_encoder.append(dep_on_self).unwrap();
        dep_len_encoder
            .append(if dep_on_self {
                c.deps().len() - 1
            } else {
                c.deps().len()
            })
            .unwrap();
    }

    // TODO: capacity
    let mut ans = Vec::with_capacity(block.len() * 15);
    let _ = leb128::write::unsigned(&mut ans, peer_register.vec().len() as u64);
    ans.extend(peer_register.vec().iter().flat_map(|p| p.to_le_bytes()));
    ans.append(&mut lengths_bytes);
    ans.append(&mut dep_self_encoder.finish().unwrap());
    ans.append(&mut dep_len_encoder.finish().unwrap());
    ans.append(&mut encoded_deps.peer_idx.finish().unwrap());
    ans.append(&mut encoded_deps.counter.finish().unwrap());
    ans.append(&mut lamport_encoder.finish().unwrap());

    let mut t = timestamp_encoder.finish().unwrap();
    let mut cml = commit_msg_len_encoder.finish().unwrap();
    let mut cms = commit_msgs.into_bytes();
    let mut meta = Vec::with_capacity(t.len() + cml.len() + cms.len());
    meta.append(&mut t);
    meta.append(&mut cml);
    meta.append(&mut cms);

    (ans, meta)
}

/// Checks that `serde_columnar`'s `DeltaOfDeltaDecoder` can read `n` values
/// from `bytes` without panicking.
///
/// The decoder of serde_columnar 0.3.14 trusts its input: a "last used bit"
/// byte above 8 makes it index past the end of the buffer, a flag bit at the
/// end of the stream makes it unwrap a missing read, and large deltas
/// overflow its `i64` sums (a panic in debug builds). This walks the same bit
/// layout with checked reads and checked arithmetic, so the decoder only
/// sees streams it can decode or reject with an error. Each value but the
/// head consumes at least one bit, so the walk is linear in `bytes`.
pub(crate) fn check_delta_of_delta(bytes: &[u8], n: usize) -> LoroResult<()> {
    let err = || LoroError::DecodeError("Decode block error: invalid delta of delta".into());
    let (head, bytes): (Option<i64>, &[u8]) =
        postcard::take_from_bytes(bytes).map_err(|_| err())?;
    let Some((&last_used_bit, bits)) = bytes.split_first() else {
        return Err(err());
    };
    if last_used_bit > 8 {
        return Err(err());
    }
    let total_bits = match bits.len() {
        0 => 0,
        len => (len - 1) * 8 + last_used_bit as usize,
    };
    let mut pos = 0usize;
    let mut read = |count: usize| -> Option<u64> {
        if total_bits - pos < count {
            return None;
        }
        let mut ans = 0u64;
        for _ in 0..count {
            let bit = (bits[pos / 8] >> (7 - pos % 8)) & 1;
            ans = (ans << 1) | bit as u64;
            pos += 1;
        }
        Some(ans)
    };

    let mut values = 0;
    let mut prev_value = 0i64;
    let mut prev_delta = 0i64;
    if let Some(head) = head {
        prev_value = head;
        values += 1;
    }
    while values < n {
        match read(1) {
            // The decoder stops here and reports too few values.
            None => return Ok(()),
            Some(0) => {}
            Some(_) => {
                let (num_bits, bias) = if read(1).ok_or_else(err)? == 0 {
                    (7, 63)
                } else if read(1).ok_or_else(err)? == 0 {
                    (9, 255)
                } else if read(1).ok_or_else(err)? == 0 {
                    (12, 2047)
                } else if read(1).ok_or_else(err)? == 0 {
                    (21, (1 << 20) - 1)
                } else {
                    (64, 0)
                };
                let delta_of_delta = (read(num_bits).ok_or_else(err)? as i64)
                    .checked_sub(bias)
                    .ok_or_else(err)?;
                prev_delta = prev_delta.checked_add(delta_of_delta).ok_or_else(err)?;
            }
        }
        prev_value = prev_value.checked_add(prev_delta).ok_or_else(err)?;
        values += 1;
    }
    Ok(())
}

pub(crate) fn decode_changes_header(
    mut bytes: &[u8],
    n_changes: usize,
    first_counter: Counter,
    counter_len: Counter,
    lamport_start: Lamport,
    lamport_len: Lamport,
) -> LoroResult<ChangesBlockHeader> {
    if n_changes == 0 {
        return Err(LoroError::DecodeError(
            "Decode block error: empty change block".into(),
        ));
    }

    let mut this_counter = first_counter;
    let peer_num = leb128::read::unsigned(&mut bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?
        as usize;
    let peers_bytes_len = peer_num
        .checked_mul(8)
        .ok_or_else(|| LoroError::DecodeError("Decode block error: peer length overflow".into()))?;
    if peer_num == 0 || bytes.len() < peers_bytes_len {
        return Err(LoroError::DecodeError(
            "Decode block error: invalid peer table".into(),
        ));
    }
    let mut peers = Vec::with_capacity(peer_num);
    for i in 0..peer_num {
        let start = 8 * i;
        let peer_id = PeerID::from_le_bytes(
            bytes[start..start + 8]
                .try_into()
                .expect("peer byte range should be checked"),
        );
        peers.push(peer_id);
    }
    let mut bytes = &bytes[peers_bytes_len..];

    // ┌───────────────────┬──────────────────────────────────────────┐    │
    // │ LEB First Counter │         N LEB128 Change AtomLen          │◁───┼─────  Important metadata
    // └───────────────────┴──────────────────────────────────────────┘    │

    let mut lengths = Vec::with_capacity(n_changes);
    for _ in 0..n_changes - 1 {
        lengths.push(leb128::read::unsigned(&mut bytes).map_err(|e| {
            LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str())
        })? as Counter);
    }
    let known_len = lengths.iter().try_fold(0i32, |acc, len| {
        acc.checked_add(*len).ok_or_else(|| {
            LoroError::DecodeError("Decode block error: counter length overflow".into())
        })
    })?;
    lengths.push(counter_len.checked_sub(known_len).ok_or_else(|| {
        LoroError::DecodeError("Decode block error: invalid counter length".into())
    })?);

    // ┌───────────────────┬────────────────────────┬─────────────────┐    │
    // │N DepOnSelf BoolRle│ N Delta Rle Deps Lens  │    N Dep IDs    │◁───┘
    // └───────────────────┴────────────────────────┴─────────────────┘

    let dep_self_decoder = BoolRleDecoder::new(bytes);
    let (dep_self, bytes) = dep_self_decoder
        .take_n_finalize(n_changes)
        .map_err(|_| LoroError::DecodeError("Decode block error: invalid deps".into()))?;
    let dep_len_decoder = AnyRleDecoder::<usize>::new(bytes);
    let (deps_len, bytes) = dep_len_decoder
        .take_n_finalize(n_changes)
        .map_err(|_| LoroError::DecodeError("Decode block error: invalid deps".into()))?;
    let other_dep_num = deps_len
        .iter()
        .try_fold(0usize, |acc, len| acc.checked_add(*len))
        .ok_or_else(|| LoroError::DecodeError("Decode block error: invalid deps".into()))?;
    let dep_peer_decoder = AnyRleDecoder::<usize>::new(bytes);
    let (dep_peers, bytes) = dep_peer_decoder
        .take_n_finalize(other_dep_num)
        .map_err(|_| LoroError::DecodeError("Decode block error: invalid deps".into()))?;
    let mut deps_peers_iter = dep_peers.into_iter();
    check_delta_of_delta(bytes, other_dep_num)?;
    let dep_counter_decoder = DeltaOfDeltaDecoder::<u32>::new(bytes)
        .map_err(|_| LoroError::DecodeError("Decode block error: invalid deps".into()))?;
    let (dep_counters, bytes) = dep_counter_decoder
        .take_n_finalize(other_dep_num)
        .map_err(|_| LoroError::DecodeError("Decode block error: invalid deps".into()))?;
    let mut deps_counters_iter = dep_counters.into_iter();
    let mut deps = Vec::with_capacity(n_changes);
    for i in 0..n_changes {
        let mut f = Frontiers::default();
        if dep_self[i] {
            let dep_counter = this_counter.checked_sub(1).ok_or_else(|| {
                LoroError::DecodeError("Decode block error: invalid self dependency".into())
            })?;
            f.push(ID::new(peers[0], dep_counter))
        }

        let len = deps_len[i];
        for _ in 0..len {
            let peer_idx = deps_peers_iter
                .next()
                .ok_or_else(|| LoroError::DecodeError("Decode block error: invalid deps".into()))?;
            let peer = peers.get(peer_idx).copied().ok_or_else(|| {
                LoroError::DecodeError("Decode block error: invalid peer index".into())
            })?;
            let counter = deps_counters_iter
                .next()
                .ok_or_else(|| LoroError::DecodeError("Decode block error: invalid deps".into()))?
                as Counter;
            f.push(ID::new(peer, counter));
        }

        deps.push(f);
        this_counter = this_counter
            .checked_add(lengths[i])
            .ok_or_else(|| LoroError::DecodeError("Decode block error: counter overflow".into()))?;
    }
    let mut counters = Vec::with_capacity(n_changes);
    let mut last = first_counter;
    for len in lengths.iter() {
        counters.push(last);
        last = last
            .checked_add(*len)
            .ok_or_else(|| LoroError::DecodeError("Decode block error: counter overflow".into()))?;
    }

    check_delta_of_delta(bytes, n_changes.saturating_sub(1))?;
    let lamport_decoder = DeltaOfDeltaDecoder::new(bytes)
        .map_err(|_| LoroError::DecodeError("Decode block error: invalid lamport".into()))?;
    let (mut lamports, rest) = lamport_decoder
        .take_n_finalize(n_changes.saturating_sub(1))
        .map_err(|_| LoroError::DecodeError("Decode block error: invalid lamport".into()))?;
    // the last lamport
    let last_len = *lengths.last().unwrap_or(&0) as u32;
    let last_lamport = lamport_start
        .checked_add(lamport_len)
        .and_then(|end| end.checked_sub(last_len))
        .ok_or_else(|| LoroError::DecodeError("Decode block error: invalid lamport".into()))?;
    lamports.push(last_lamport as Lamport);

    // we need counter range, so encode
    counters.push(
        first_counter
            .checked_add(counter_len)
            .ok_or_else(|| LoroError::DecodeError("Decode block error: counter overflow".into()))?,
    );
    if !rest.is_empty() {
        return Err(LoroError::DecodeError(
            "Decode block error: trailing bytes in header".into(),
        ));
    }

    Ok(ChangesBlockHeader {
        peer: peers[0],
        counter: first_counter,
        n_changes,
        peers,
        counters,
        deps_groups: deps,
        lamports,
        keys: OnceCell::new(),
        cids: OnceCell::new(),
    })
}

struct EncodedDeps {
    peer_idx: AnyRleEncoder<u32>,
    counter: DeltaOfDeltaEncoder,
}
