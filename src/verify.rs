use bit_set::Bitset;
use diag::Diagnostic;
use nameck::Atom;
use nameck::Nameset;
use parser;
use parser::Comparer;
use parser::copy_token;
use parser::NO_STATEMENT;
use parser::Segment;
use parser::SegmentId;
use parser::SegmentOrder;
use parser::SegmentRef;
use parser::StatementAddress;
use parser::StatementRef;
use parser::StatementType;
use parser::TokenPtr;
use scopeck::ExprFragment;
use scopeck::Frame;
use scopeck::Hyp;
use scopeck::ScopeReader;
use scopeck::ScopeResult;
use scopeck::ScopeUsage;
use scopeck::VerifyExpr;
use segment_set::SegmentSet;
use std::cmp::Ordering;
use std::mem;
use std::ops::Range;
use std::sync::Arc;
use std::u32;
use std::usize;
use util::copy_portion;
use util::fast_clear;
use util::fast_extend;
use util::HashMap;
use util::new_map;
use util::ptr_eq;

enum PreparedStep<'a> {
    Hyp(Bitset, Atom, Range<usize>),
    Assert(&'a Frame),
}

struct StackSlot {
    vars: Bitset,
    code: Atom,
    expr: Range<usize>,
}

struct VerifyState<'a> {
    this_seg: SegmentRef<'a>,
    order: &'a SegmentOrder,
    nameset: &'a Nameset,
    scoper: ScopeReader<'a>,
    cur_frame: &'a Frame,
    prepared: Vec<PreparedStep<'a>>,
    stack: Vec<StackSlot>,
    stack_buffer: Vec<u8>,
    temp_buffer: Vec<u8>,
    subst_info: Vec<(Range<usize>, Bitset)>,
    var2bit: HashMap<Atom, usize>,
    dv_map: &'a [Bitset],
}

fn map_var<'a>(state: &mut VerifyState<'a>, token: Atom) -> usize {
    let nbit = state.var2bit.len();
    *state.var2bit.entry(token).or_insert(nbit)
}

// the initial hypotheses are accessed directly to avoid having to look up their names
fn prepare_hypothesis<'a>(state: &mut VerifyState, hyp: &'a Hyp) {
    let mut vars = Bitset::new();
    let tos = state.stack_buffer.len();

    if hyp.is_float() {
        fast_extend(&mut state.stack_buffer,
                    state.nameset.atom_name(state.cur_frame.var_list[hyp.variable_index]));
        *state.stack_buffer.last_mut().unwrap() |= 0x80;
        vars.set_bit(hyp.variable_index); // and we have prior knowledge it's identity mapped
    } else {
        for part in &*hyp.expr.tail {
            fast_extend(&mut state.stack_buffer,
                        &state.cur_frame.const_pool[part.prefix.clone()]);
            fast_extend(&mut state.stack_buffer,
                        state.nameset.atom_name(state.cur_frame.var_list[part.var]));
            *state.stack_buffer.last_mut().unwrap() |= 0x80;
            vars.set_bit(part.var); // and we have prior knowledge it's identity mapped
        }
        fast_extend(&mut state.stack_buffer,
                    &state.cur_frame.const_pool[hyp.expr.rump.clone()]);
    }

    let ntos = state.stack_buffer.len();
    state.prepared.push(PreparedStep::Hyp(vars, hyp.expr.typecode, tos..ntos));
}

/// Adds a named $e hypothesis to the prepared array.  These are not kept in the frame
/// array due to infrequent use, so other measures are needed.
fn prepare_named_hyp(state: &mut VerifyState, label: TokenPtr) -> Option<Diagnostic> {
    for hyp in &*state.cur_frame.hypotheses {
        if hyp.is_float() {
            continue;
        }
        assert!(hyp.address.segment_id == state.this_seg.id);
        if state.this_seg.statement(hyp.address.index).label() == label {
            prepare_hypothesis(state, hyp);
            return None;
        }
    }
    return Some(Diagnostic::StepMissing(copy_token(label)));
}

fn prepare_step(state: &mut VerifyState, label: TokenPtr) -> Option<Diagnostic> {
    let frame = match state.scoper.get(label) {
        Some(fp) => fp,
        None => {
            return prepare_named_hyp(state, label);
        }
    };

    let valid = frame.valid;
    let pos = state.cur_frame.valid.start;
    if state.order.cmp(&pos, &valid.start) != Ordering::Greater {
        return Some(Diagnostic::StepUsedBeforeDefinition(copy_token(label)));
    }

    if valid.end != NO_STATEMENT {
        if pos.segment_id != valid.start.segment_id || pos.index >= valid.end {
            return Some(Diagnostic::StepUsedAfterScope(copy_token(label)));
        }
    }

    if frame.stype == StatementType::Axiom || frame.stype == StatementType::Provable {
        state.prepared.push(PreparedStep::Assert(frame));
    } else {
        let mut vars = Bitset::new();

        for &var in &*frame.var_list {
            vars.set_bit(map_var(state, var));
        }

        let tos = state.stack_buffer.len();
        fast_extend(&mut state.stack_buffer, &frame.stub_expr);
        let ntos = state.stack_buffer.len();
        state.prepared
            .push(PreparedStep::Hyp(vars, frame.target.typecode, tos..ntos));
    }

    return None;
}

fn do_substitute(target: &mut Vec<u8>,
                 frame: &Frame,
                 expr: &VerifyExpr,
                 vars: &[(Range<usize>, Bitset)]) {
    for part in &*expr.tail {
        fast_extend(target, &frame.const_pool[part.prefix.clone()]);
        copy_portion(target, vars[part.var].0.clone());
    }
    fast_extend(target, &frame.const_pool[expr.rump.clone()]);
}

fn do_substitute_eq(mut compare: &[u8],
                    frame: &Frame,
                    expr: &VerifyExpr,
                    vars: &[(Range<usize>, Bitset)],
                    var_buffer: &[u8])
                    -> bool {
    fn step(compare: &mut &[u8], slice: &[u8]) -> bool {
        let len = slice.len();
        if (*compare).len() < len {
            return true;
        }
        if slice != &(*compare)[0..len] {
            return true;
        }
        *compare = &(*compare)[len..];
        return false;
    }

    for part in &*expr.tail {
        if step(&mut compare, &frame.const_pool[part.prefix.clone()]) {
            return false;
        }
        if step(&mut compare, &var_buffer[vars[part.var].0.clone()]) {
            return false;
        }
    }

    if step(&mut compare, &frame.const_pool[expr.rump.clone()]) {
        return false;
    }

    return compare.is_empty();
}

fn do_substitute_raw(target: &mut Vec<u8>, frame: &Frame, nameset: &Nameset) {
    for part in &*frame.target.tail {
        fast_extend(target, &frame.const_pool[part.prefix.clone()]);
        fast_extend(target, nameset.atom_name(frame.var_list[part.var]));
        *target.last_mut().unwrap() |= 0x80;
    }
    fast_extend(target, &frame.const_pool[frame.target.rump.clone()]);
}

fn do_substitute_vars(expr: &[ExprFragment], vars: &[(Range<usize>, Bitset)]) -> Bitset {
    let mut out = Bitset::new();
    for part in expr {
        out |= &vars[part.var].1;
    }
    out
}

fn execute_step(state: &mut VerifyState, index: usize) -> Option<Diagnostic> {
    if index >= state.prepared.len() {
        return Some(Diagnostic::StepOutOfRange);
    }

    let fref = match state.prepared[index] {
        PreparedStep::Hyp(ref vars, code, ref expr) => {
            state.stack.push(StackSlot {
                vars: vars.clone(),
                code: code,
                expr: expr.clone(),
            });
            return None;
        }
        PreparedStep::Assert(fref) => fref,
    };

    if state.stack.len() < fref.hypotheses.len() {
        return Some(Diagnostic::ProofUnderflow);
    }
    let sbase = state.stack.len() - fref.hypotheses.len();

    while state.subst_info.len() < fref.mandatory_count {
        // this is mildly unhygenic, since slots corresponding to $e hyps won't get cleared, but
        // scopeck shouldn't generate references to them
        state.subst_info.push((0..0, Bitset::new()));
    }

    // check $f, build substitution
    // check $e
    // metamath spec guarantees $f will always come before any $e they affect (!)
    for (ix, hyp) in fref.hypotheses.iter().enumerate() {
        let slot = &state.stack[sbase + ix];

        // schedule a memory ref and nice predicable branch before the ugly branch
        if slot.code != hyp.expr.typecode {
            if hyp.is_float() {
                return Some(Diagnostic::StepFloatWrongType);
            } else {
                return Some(Diagnostic::StepEssenWrongType);
            }
        }

        if hyp.is_float() {
            state.subst_info[hyp.variable_index] = (slot.expr.clone(), slot.vars.clone());
        } else {
            if !do_substitute_eq(&state.stack_buffer[slot.expr.clone()],
                                 fref,
                                 &hyp.expr,
                                 &state.subst_info,
                                 &state.stack_buffer) {
                return Some(Diagnostic::StepEssenWrong);
            }
        }
    }

    let tos = state.stack_buffer.len();
    do_substitute(&mut state.stack_buffer,
                  fref,
                  &fref.target,
                  &state.subst_info);
    let ntos = state.stack_buffer.len();

    state.stack.truncate(sbase);
    state.stack.push(StackSlot {
        code: fref.target.typecode,
        vars: do_substitute_vars(&fref.target.tail, &state.subst_info),
        expr: tos..ntos,
    });

    // check $d
    for &(ix1, ix2) in &*fref.mandatory_dv {
        for var1 in &state.subst_info[ix1].1 {
            for var2 in &state.subst_info[ix2].1 {
                if var1 >= state.dv_map.len() || !state.dv_map[var1].has_bit(var2) {
                    return Some(Diagnostic::ProofDvViolation);
                }
            }
        }
    }

    return None;
}

fn finalize_step(state: &mut VerifyState) -> Option<Diagnostic> {
    if state.stack.len() == 0 {
        return Some(Diagnostic::ProofNoSteps);
    }
    if state.stack.len() > 1 {
        return Some(Diagnostic::ProofExcessEnd);
    }
    let tos = state.stack.last().unwrap();

    if tos.code != state.cur_frame.target.typecode {
        return Some(Diagnostic::ProofWrongTypeEnd);
    }

    fast_clear(&mut state.temp_buffer);
    do_substitute_raw(&mut state.temp_buffer, &state.cur_frame, state.nameset);

    if state.stack_buffer[tos.expr.clone()] != state.temp_buffer[..] {
        return Some(Diagnostic::ProofWrongExprEnd);
    }

    None
}

fn save_step(state: &mut VerifyState) {
    let top = state.stack.last().expect("can_save should prevent getting here");
    state.prepared.push(PreparedStep::Hyp(top.vars.clone(), top.code, top.expr.clone()));
}

// proofs are not self-synchronizing, so it's not likely to get >1 usable error
fn verify_proof<'a>(state: &mut VerifyState<'a>, stmt: StatementRef<'a>) -> Option<Diagnostic> {
    // only intend to check $p statements
    if stmt.statement.stype != StatementType::Provable {
        return None;
    }

    // no valid frame -> no use checking
    // may wish to record a secondary error?
    let cur_frame = match state.scoper.get(stmt.label()) {
        None => return None,
        Some(x) => x,
    };

    state.cur_frame = cur_frame;
    state.stack.clear();
    fast_clear(&mut state.stack_buffer);
    state.prepared.clear();
    state.var2bit.clear();
    state.dv_map = &cur_frame.optional_dv;
    // temp_buffer and subst_info are cleared before use

    for (index, &tokr) in cur_frame.var_list.iter().enumerate() {
        state.var2bit.insert(tokr, index);
    }

    if stmt.proof_slice_at(0) == b"(" {
        let mut i = 1;

        for hyp in &*cur_frame.hypotheses {
            prepare_hypothesis(state, hyp);
        }

        loop {
            if i >= stmt.proof_len() {
                return Some(Diagnostic::ProofUnterminatedRoster);
            }
            let chunk = stmt.proof_slice_at(i);
            i += 1;

            if chunk == b")" {
                break;
            }

            if let Some(err) = prepare_step(state, chunk) {
                return Some(err);
            }
        }

        let mut k = 0usize;
        let mut can_save = false;
        while i < stmt.proof_len() {
            let chunk = stmt.proof_slice_at(i);
            for &ch in chunk {
                if ch >= b'A' && ch <= b'T' {
                    k = k * 20 + (ch - b'A') as usize;
                    if let Some(err) = execute_step(state, k) {
                        return Some(err);
                    }
                    k = 0;
                    can_save = true;
                } else if ch >= b'U' && ch <= b'Y' {
                    k = k * 5 + 1 + (ch - b'U') as usize;
                    if k >= (u32::max_value() as usize / 20) - 1 {
                        return Some(Diagnostic::ProofMalformedVarint);
                    }
                    can_save = false;
                } else if ch == b'Z' {
                    if !can_save {
                        return Some(Diagnostic::ProofInvalidSave);
                    }
                    save_step(state);
                    can_save = false;
                } else if ch == b'?' {
                    if k > 0 {
                        return Some(Diagnostic::ProofMalformedVarint);
                    }
                    return Some(Diagnostic::ProofIncomplete);
                }
            }
            i += 1;
        }

        if k > 0 {
            return Some(Diagnostic::ProofMalformedVarint);
        }
    } else {
        let mut count = 0;
        for i in 0..stmt.proof_len() {
            let chunk = stmt.proof_slice_at(i);
            if chunk == b"?" {
                return Some(Diagnostic::ProofIncomplete);
            } else {
                if let Some(err) = prepare_step(state, chunk) {
                    return Some(err);
                }
                if let Some(err) = execute_step(state, count) {
                    return Some(err);
                }
                count += 1;
            }
        }
    }

    if let Some(err) = finalize_step(state) {
        return Some(err);
    }

    return None;
}

struct VerifySegment {
    source: Arc<Segment>,
    scope_usage: ScopeUsage,
    diagnostics: HashMap<StatementAddress, Diagnostic>,
}

#[derive(Default,Clone)]
pub struct VerifyResult {
    segments: HashMap<SegmentId, Arc<VerifySegment>>,
}

impl VerifyResult {
    pub fn diagnostics(&self) -> Vec<(StatementAddress, Diagnostic)> {
        let mut out = Vec::new();
        for vsr in self.segments.values() {
            for (&sa, &ref diag) in &vsr.diagnostics {
                out.push((sa, diag.clone()));
            }
        }
        out
    }
}

fn verify_segment(sset: &SegmentSet,
                  nset: &Nameset,
                  scopes: &ScopeResult,
                  sid: SegmentId)
                  -> VerifySegment {
    let mut diagnostics = new_map();
    let dummy_frame = Frame::default();
    let sref = sset.segment(sid);
    let mut state = VerifyState {
        this_seg: sref,
        scoper: ScopeReader::new(scopes),
        nameset: nset,
        order: &sset.order,
        cur_frame: &dummy_frame,
        stack: Vec::new(),
        stack_buffer: Vec::new(),
        prepared: Vec::new(),
        temp_buffer: Vec::new(),
        subst_info: Vec::new(),
        var2bit: new_map(),
        dv_map: &dummy_frame.optional_dv,
    };
    for stmt in sref.statement_iter() {
        if let Some(diag) = verify_proof(&mut state, stmt) {
            diagnostics.insert(stmt.address(), diag);
        }
    }
    VerifySegment {
        source: sref.segment.clone(),
        diagnostics: diagnostics,
        scope_usage: state.scoper.into_usage(),
    }
}

pub fn verify(result: &mut VerifyResult,
              segments: &Arc<SegmentSet>,
              nset: &Arc<Nameset>,
              scope: &Arc<ScopeResult>) {
    let old = mem::replace(&mut result.segments, new_map());
    let mut ssrq = Vec::new();
    for sref in segments.segments() {
        let segments2 = segments.clone();
        let nset = nset.clone();
        let scope = scope.clone();
        let id = sref.id;
        let old_res_o = old.get(&id).cloned();
        ssrq.push(segments.exec.exec(sref.bytes(), move || {
            let sref = segments2.segment(id);
            if let Some(old_res) = old_res_o {
                if old_res.scope_usage.valid(&nset, &scope) &&
                   ptr_eq::<Segment>(&old_res.source, sref.segment) {
                    return (id, old_res.clone());
                }
            }
            if segments2.options.trace_recalc {
                println!("verify({:?})",
                         parser::guess_buffer_name(&sref.segment.buffer));
            }
            (id, Arc::new(verify_segment(&segments2, &nset, &scope, id)))
        }))
    }

    result.segments.clear();
    for promise in ssrq {
        let (id, arc) = promise.wait();
        result.segments.insert(id, arc);
    }
}
