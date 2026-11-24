//! Wave-uniform values used by the DXIL lowering.

use std::collections::HashSet;

use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{Inst, Program, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit, Structured};

/// `ds_read*` sites `(block, inst)` whose shared memory is written only earlier in the
/// program, separated from the read by an `s_barrier`, with no backward branch from or
/// after the read looping back over an LDS write.
fn lds_stable_reads(p: &Program) -> HashSet<(usize, usize)> {
    if p.blocks
        .iter()
        .any(|b| matches!(b.exit, crate::ir::Exit::Indirect))
    {
        return HashSet::new();
    }
    let is_read = |inst: &Inst| {
        matches!(
            &inst.fields,
            Fields::Ds {
                addr: Some(_),
                data,
                gds: false,
                ..
            } if inst.name().starts_with("ds_read") && data.is_empty()
        )
    };
    let is_write = |inst: &Inst| {
        matches!(inst.fields, Fields::Ds { gds: false, .. })
            && !(inst.name().starts_with("ds_swizzle") || inst.name().contains("permute"))
            && !is_read(inst)
    };
    let rpo = crate::ir::reverse_postorder(&p.blocks);
    let mut pos = vec![usize::MAX; p.blocks.len()];
    for (idx, &b) in rpo.iter().enumerate() {
        pos[b] = idx;
    }
    let mut max_write: Option<(usize, usize)> = None;
    for &b in &rpo {
        for (i, inst) in p.blocks[b].insts.iter().enumerate() {
            if is_write(inst) {
                max_write = Some(max_write.map_or((pos[b], i), |m| m.max((pos[b], i))));
            }
        }
    }
    let mut dirty_in = vec![false; p.blocks.len()];
    if max_write.is_some() {
        let mut changed = true;
        while changed {
            changed = false;
            for &b in &rpo {
                let mut d = dirty_in[b];
                for inst in &p.blocks[b].insts {
                    if is_write(inst) {
                        d = true;
                    } else if inst.name() == "s_barrier" {
                        d = false;
                    }
                }
                if d {
                    for succ in p.blocks[b].exit.successors() {
                        if !dirty_in[succ] {
                            dirty_in[succ] = true;
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    let back_over_write_from: Vec<usize> = match max_write {
        Some((mw, _)) => rpo
            .iter()
            .copied()
            .filter(|&from| {
                p.blocks[from]
                    .exit
                    .successors()
                    .into_iter()
                    .any(|to| pos[to] <= pos[from] && pos[to] <= mw)
            })
            .map(|from| pos[from])
            .collect(),
        None => Vec::new(),
    };
    let mut out = HashSet::new();
    for &b in &rpo {
        let loops_over_write = back_over_write_from.iter().any(|&f| f >= pos[b]);
        let mut d = dirty_in[b];
        for (i, inst) in p.blocks[b].insts.iter().enumerate() {
            if is_write(inst) {
                d = true;
            } else if inst.name() == "s_barrier" {
                d = false;
            } else if is_read(inst)
                && !d
                && !loops_over_write
                && max_write.is_none_or(|mw| (pos[b], i) > mw)
            {
                out.insert((b, i));
            }
        }
    }
    out
}

/// VGPR values that are the same in every lane holding them. `all` flags blocks
/// that run in every lane.
/// written by an instruction every arriving lane runs (no guard), lane by lane, from scalars,
/// constants and such values; or loaded from an address made of them. The compiler keeps
/// wave-uniform data in VGPRs and fetches it with `v_readfirstlane`.
///
/// Three more facts are proved, each only where no lane of a wave can part from the others:
/// - the wave number (`tid >> 6` of the entry `v0`, in a group whose x size is a multiple of
///   64) is the same in all lanes of a console wave;
/// - an LDS read (`ds_read_b32`) at such an address is;
/// - a value joined by a phi is, also at a loop header, when its arguments are and every
///   predecessor is entered by all of the arriving lanes or none (the lanes' EXEC there is
///   made of conditions every lane agrees on: SCC values, compares of such values, and the
///   loop-carried masks made of them). That is a greatest fixed point: a loop-carried value
///   is taken to be alike and the assumption is dropped once an argument is not.
///
/// LDS reads also require a barrier after the last write to LDS.
pub(super) fn lane_uniform(
    p: &Program,
    st: &Structured,
    all: &[bool],
    threads: Option<[u32; 3]>,
) -> HashSet<Value> {
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    let stable_lds = lds_stable_reads(p);
    let entry_v0 = p.inputs.iter().find(|(r, _)| *r == Reg::V(0)).map(|x| x.1);
    let waves_whole = threads.is_some_and(|t| t[0] != 0 && t[0].is_multiple_of(64));
    // the V-register phis, and the lane-mask phis, assumed alike until shown not to be
    let mut assumed_v: HashSet<Value> = HashSet::new();
    let mut assumed_m: HashSet<Value> = HashSet::new();
    for block in &p.blocks {
        for phi in &block.phis {
            if matches!(phi.reg, Reg::V(_)) {
                assumed_v.insert(phi.value);
            }
            if st.masks.contains_key(&phi.value) {
                assumed_m.insert(phi.value);
            }
        }
    }
    // Whether a condition is the same in every lane of the wave.
    let uniform_bool =
        |b: Bool, same: &HashSet<Value>, cmp: &HashSet<Value>, masks: &HashSet<Value>| {
            st.bools.support(b).iter().all(|a| match a {
                Atom::Uniform(_) => true,
                Atom::Lane(v) => cmp.contains(v) || same.contains(v),
                Atom::Phi(v) => masks.contains(v),
                _ => false,
            })
        };
    loop {
        let mut same: HashSet<Value> = HashSet::new();
        // values written by compares whose operands are all alike (the lane's bit is the wave's)
        let mut cmp: HashSet<Value> = HashSet::new();
        for b in crate::ir::reverse_postorder(&p.blocks) {
            let block = &p.blocks[b];
            let whole = block
                .preds
                .iter()
                .all(|&q| all.get(q).copied().unwrap_or(false));
            // every predecessor is entered by all the arriving lanes or none of them
            let together = block.preds.iter().all(|&q| {
                uniform_bool(st.reach[q], &same, &cmp, &assumed_m)
                    && st.guards[q]
                        .iter()
                        .all(|g| (*g).is_none_or(|g| uniform_bool(g, &same, &cmp, &assumed_m)))
                    && match &st.exits[q] {
                        LaneExit::Branch { when, .. } => {
                            uniform_bool(*when, &same, &cmp, &assumed_m)
                        }
                        _ => true,
                    }
            });
            for phi in &block.phis {
                if !matches!(phi.reg, Reg::V(_)) {
                    continue;
                }
                if whole && phi.args.iter().all(|a| same.contains(a)) {
                    same.insert(phi.value);
                } else if together && assumed_v.contains(&phi.value) {
                    // its arguments are checked once the pass is done
                    same.insert(phi.value);
                }
            }
            for (i, inst) in block.insts.iter().enumerate() {
                let name = inst.name();
                let alike_ops = |ops: &[Operand], same: &HashSet<Value>| {
                    ops.iter().all(|op| match *op {
                        Operand::Vector { reg, .. } => {
                            // every dword of the operand the instruction reads from that register on
                            let read: Vec<Value> = inst
                                .reads
                                .iter()
                                .filter(|r| matches!(r.0, Reg::V(x) if x >= reg))
                                .map(|r| r.1)
                                .collect();
                            !read.is_empty() && read.iter().all(|x| same.contains(x))
                        }
                        // a scalar is one value; a lane mask is not, unless a compare of such values made it
                        Operand::Scalar { code, .. } if code != NULL => inst
                            .read(Reg::S(code))
                            .is_none_or(|x| !is_mask(x) || same.contains(&x)),
                        _ => true,
                    })
                };
                // a read of the group's LDS at an address the lanes agree on
                if let Fields::Ds {
                    addr: Some(addr), ..
                } = &inst.fields
                    && stable_lds.contains(&(b, i))
                    && alike_ops(&[*addr], &same)
                    && st.guards[b][i].is_none_or(|g| uniform_bool(g, &same, &cmp, &assumed_m))
                {
                    same.extend(inst.writes.iter().filter(|w| w.0 != Reg::Scc).map(|w| w.1));
                    continue;
                }
                let ops: Vec<Operand> = match &inst.fields {
                    Fields::Valu(v) if v.dpp.is_none() && v.sdwa.is_none() => v.src.clone(),
                    _ => continue,
                };
                if name.starts_with("v_cmp") {
                    if alike_ops(&ops, &same) {
                        cmp.extend(inst.writes.iter().map(|w| w.1));
                    }
                    if name.starts_with("v_cmpx") {
                        continue;
                    }
                } else if name.starts_with("v_mbcnt") || name.contains("lane") {
                    continue;
                }
                // `tid >> k` (k at least 6) of the entry thread number: the wave's
                if waves_whole
                    && name == "v_lshrrev_b32"
                    && let [Operand::Const(134..=159), Operand::Vector { reg: 0, .. }] = &ops[..]
                    && let Some(v0) = entry_v0
                    && inst.read(Reg::V(0)) == Some(v0)
                    && st.guards[b][i].is_none_or(|g| uniform_bool(g, &same, &cmp, &assumed_m))
                {
                    same.extend(inst.writes.iter().filter(|w| w.0 != Reg::Scc).map(|w| w.1));
                    continue;
                }
                // lanes outside EXEC keep another value, unless the lanes agree on running it
                if st.guards[b][i].is_some_and(|g| !uniform_bool(g, &same, &cmp, &assumed_m)) {
                    continue;
                }
                if alike_ops(&ops, &same) {
                    same.extend(inst.writes.iter().filter(|w| w.0 != Reg::Scc).map(|w| w.1));
                }
            }
        }
        // what was assumed of the loop-carried values: dropped where it did not hold
        let mut changed = false;
        for block in &p.blocks {
            for phi in &block.phis {
                if assumed_v.contains(&phi.value)
                    && same.contains(&phi.value)
                    && !phi.args.iter().all(|a| same.contains(a))
                {
                    assumed_v.remove(&phi.value);
                    changed = true;
                }
                if assumed_m.contains(&phi.value)
                    && !phi.args.iter().all(|a| {
                        st.masks
                            .get(a)
                            .is_some_and(|&m| uniform_bool(m, &same, &cmp, &assumed_m))
                    })
                {
                    assumed_m.remove(&phi.value);
                    changed = true;
                }
            }
        }
        if !changed {
            return same;
        }
    }
}
