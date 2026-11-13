// Lifted from rudb crates/rudb-qc-ir/src/build.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`. `Mix::write` reads its words with `as_chunks`.

//! The builder: appends instructions to a function, folding and deduplicating as it goes.
//!
//! Section 6.11 of `spec/compiler/06-qir.md` puts constant folding, the identities that NULL
//! specialization produces, and CSE at append time, so that no pass has to find them later. CSE
//! here is scoped to the current block and the entry block, both of which dominate the append
//! point, so a hit is always a dominating definition. That is narrower than the structured
//! scopes of the spec and it needs no bookkeeping from the generator. It can be widened when a
//! measurement says the extra hits matter.
//!
//! Every appending method is `#[track_caller]`, and the caller's location becomes the
//! instruction's provenance (section 6.12).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::panic::Location;

use crate::ir::eval;
use crate::ir::func::{A16, Const, DEAD, INV, NT, header};
use crate::ir::{Block, BlockData, Class, Form, Func, Op, Site, Ty, Val, ValInfo};

impl Func {
    /// An empty function whose entry block has the ABI parameters `(ptr %st, ptr %m)`.
    #[must_use]
    pub fn new(name: &str, version: &str, plan: u32) -> Func {
        let mut f = Func {
            name: name.to_owned(),
            version: version.to_owned(),
            plan,
            sites: vec![Site { plan: 0, file: "", line: 0 }],
            ..Func::default()
        };
        let entry = f.add_block();
        let st = f.new_val(Ty::Ptr, Some("st"));
        let m = f.new_val(Ty::Ptr, Some("m"));
        f.blocks[entry.index()].params = vec![st, m];
        f
    }

    /// Adds an empty block.
    pub fn add_block(&mut self) -> Block {
        // Room for a dozen or so instructions from the start, so a block's code is not grown
        // again and again as the generator appends to it.
        let code = Vec::with_capacity(64);
        let prov = Vec::with_capacity(16);
        self.blocks.push(BlockData { code, prov, ..BlockData::default() });
        Block((self.blocks.len() - 1) as u32)
    }

    /// Allocates a value number.
    pub fn new_val(&mut self, ty: Ty, name: Option<&str>) -> Val {
        self.vals.push(ValInfo { ty, class: Class::default(), name: name.map(Into::into) });
        Val((self.vals.len() - 1) as u32)
    }

    /// Adds a pool entry without looking for an equal one.
    pub fn add_const(&mut self, ty: Ty, bits: u128) -> Val {
        self.consts.push(Const { ty, bits: bits & ty.mask() });
        Val::konst((self.consts.len() - 1) as u32)
    }

    /// Appends an instruction's words to block `b` as they are, with no folding. The parser and
    /// the builder both end here.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        b: Block,
        op: Op,
        ty: Ty,
        flags: u32,
        result: Option<Val>,
        ops: &[u32],
        site: u32,
    ) {
        let block = &mut self.blocks[b.index()];
        block.code.push(header(op, ty, flags, ops.len() + usize::from(result.is_some())));
        if let Some(r) = result {
            block.code.push(r.0);
        }
        block.code.extend_from_slice(ops);
        block.prov.push(site);
    }
}

/// Appends to one function.
#[derive(Debug)]
pub struct Builder {
    f: Func,
    cur: Block,
    plan: u32,
    sites: HashMap<(usize, u32), u32, Quick>,
    consts: HashMap<Const, Val, Quick>,
    cse: HashMap<Key, (Block, Val), Quick>,
    key: Vec<u32>,
    fold: bool,
}

/// The builder's maps are keyed by what the generator made, never by user data, so they hash
/// with a multiply and rotate instead of `SipHash`.
type Quick = BuildHasherDefault<Mix>;

/// The most words of an instruction a CSE key holds in place.
const SHORT: usize = 6;

/// A CSE key: an instruction's header and operands. Nearly every pure instruction has a header
/// and at most five operands, so its key is kept in place and not boxed, which saved an
/// allocation for each one the generator appended.
#[derive(Debug, PartialEq, Eq, Hash)]
enum Key {
    Short(u8, [u32; SHORT]),
    Long(Box<[u32]>),
}

impl Key {
    fn of(words: &[u32]) -> Key {
        if words.len() > SHORT {
            return Key::Long(words.into());
        }
        let mut short = [0; SHORT];
        short[..words.len()].copy_from_slice(words);
        Key::Short(words.len() as u8, short)
    }
}

#[derive(Default)]
struct Mix(u64);

impl Mix {
    fn add(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for Mix {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut w = [0; 8];
            w[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(w));
        }
    }

    fn write_u8(&mut self, x: u8) {
        self.add(u64::from(x));
    }

    fn write_u32(&mut self, x: u32) {
        self.add(u64::from(x));
    }

    fn write_u64(&mut self, x: u64) {
        self.add(x);
    }

    fn write_usize(&mut self, x: usize) {
        self.add(x as u64);
    }
}

impl Builder {
    /// Starts a function. The entry block is current.
    #[must_use]
    pub fn new(name: &str, version: &str, plan: u32) -> Builder {
        Builder {
            f: Func::new(name, version, plan),
            cur: Block(0),
            plan,
            sites: HashMap::default(),
            consts: HashMap::default(),
            cse: HashMap::default(),
            key: Vec::new(),
            fold: true,
        }
    }

    /// Turns folding and CSE off, so that every call appends exactly one instruction. Tests of
    /// the verifier and the backends use this to build a module that says what it says.
    #[must_use]
    pub fn literal(mut self) -> Builder {
        self.fold = false;
        self
    }

    /// The function so far.
    #[must_use]
    pub fn func(&self) -> &Func {
        &self.f
    }

    /// The function so far, for the generator's own tables: the state layout, the sink flag.
    pub fn func_mut(&mut self) -> &mut Func {
        &mut self.f
    }

    /// Ends the function.
    #[must_use]
    pub fn finish(self) -> Func {
        self.f
    }

    /// The state pointer, `%st`.
    #[must_use]
    pub fn st(&self) -> Val {
        self.f.blocks[0].params[0]
    }

    /// The morsel pointer, `%m`.
    #[must_use]
    pub fn m(&self) -> Val {
        self.f.blocks[0].params[1]
    }

    /// The plan node the following instructions come from.
    pub fn set_plan(&mut self, plan: u32) {
        self.plan = plan;
    }

    /// Adds a block with parameters of these types and names.
    pub fn block(&mut self, params: &[(Ty, &str)]) -> Block {
        let b = self.f.add_block();
        let params = params.iter().map(|(ty, name)| self.f.new_val(*ty, Some(name))).collect();
        self.f.blocks[b.index()].params = params;
        b
    }

    /// Parameter `i` of block `b`.
    #[must_use]
    pub fn param(&self, b: Block, i: usize) -> Val {
        self.f.blocks[b.index()].params[i]
    }

    /// Makes `b` the block that instructions are appended to.
    pub fn switch_to(&mut self, b: Block) {
        self.cur = b;
    }

    /// The block that instructions are appended to.
    #[must_use]
    pub fn current(&self) -> Block {
        self.cur
    }

    /// Whether the current block already ends in a terminator.
    #[must_use]
    pub fn terminated(&self) -> bool {
        self.f.terminator(self.cur).is_some_and(|i| i.op.is_terminator())
    }

    /// Declares `b` a loop header at nesting depth `depth`.
    pub fn set_loop(&mut self, b: Block, depth: u8) {
        let data = &mut self.f.blocks[b.index()];
        data.is_loop = true;
        data.depth = depth;
    }

    /// Declares the loop at `b` bounded by a constant trip count.
    pub fn set_bounded(&mut self, b: Block) {
        self.f.blocks[b.index()].bounded = true;
    }

    /// Declares the loop at `b` the morsel batch loop.
    pub fn set_batch(&mut self, b: Block) {
        self.f.blocks[b.index()].batch = true;
    }

    /// Declares `b` rarely taken.
    pub fn set_cold(&mut self, b: Block) {
        self.f.blocks[b.index()].cold = true;
    }

    /// Gives a value a name for the printer.
    pub fn name(&mut self, v: Val, name: &str) {
        if !v.is_const() {
            self.f.vals[v.index()].name = Some(name.into());
        }
    }

    /// Sets the storage class of a `str16` value.
    pub fn set_class(&mut self, v: Val, class: Class) {
        if !v.is_const() {
            self.f.vals[v.index()].class = class;
        }
    }

    /// Records that `valid` is the validity of `v`, for rule V12.
    pub fn set_validity(&mut self, v: Val, valid: Val) {
        if valid != self.bool(true) {
            self.f.validity.push((v, valid));
        }
    }

    /// The type of a value.
    #[must_use]
    pub fn ty(&self, v: Val) -> Ty {
        self.f.ty(v)
    }

    // Constants.

    /// A constant with these bits.
    pub fn konst(&mut self, ty: Ty, bits: u128) -> Val {
        let c = Const { ty, bits: bits & ty.mask() };
        if let Some(v) = self.consts.get(&c) {
            return *v;
        }
        let v = self.f.add_const(ty, c.bits);
        self.consts.insert(c, v);
        v
    }

    /// An integer constant.
    pub fn int(&mut self, ty: Ty, x: i128) -> Val {
        self.konst(ty, x as u128)
    }

    /// An `i1` constant.
    pub fn bool(&mut self, b: bool) -> Val {
        self.konst(Ty::I1, u128::from(b))
    }

    /// An `f64` constant.
    pub fn f64(&mut self, x: f64) -> Val {
        self.konst(Ty::F64, u128::from(x.to_bits()))
    }

    /// An `f32` constant.
    pub fn f32(&mut self, x: f32) -> Val {
        self.konst(Ty::F32, u128::from(x.to_bits()))
    }

    fn bits(&self, v: Val) -> Option<u128> {
        self.f.constant(v).map(|c| c.bits)
    }

    // The append path.

    #[track_caller]
    fn site(&mut self) -> u32 {
        let loc = Location::caller();
        let key = (loc as *const Location<'static> as usize, self.plan);
        if let Some(id) = self.sites.get(&key) {
            return *id;
        }
        self.f.sites.push(Site { plan: self.plan, file: loc.file(), line: loc.line() });
        let id = (self.f.sites.len() - 1) as u32;
        self.sites.insert(key, id);
        id
    }

    /// Appends one instruction and returns its result, deduplicating it when it is pure or an
    /// invariant load.
    #[track_caller]
    fn emit(&mut self, op: Op, ty: Ty, flags: u32, ops: &[u32]) -> Option<Val> {
        let rty = op.result(ty);
        let cse = self.fold && rty != Ty::Void && (op.is_pure() || flags & INV != 0);
        if cse {
            self.key.clear();
            self.key.push(header(op, ty, flags, ops.len()));
            self.key.extend_from_slice(ops);
            if let Some(&(b, v)) = self.cse.get(&Key::of(&self.key))
                && (b == self.cur || b == Block(0))
            {
                return Some(v);
            }
        }
        let result = (rty != Ty::Void).then(|| self.f.new_val(rty, None));
        let site = self.site();
        self.f.push(self.cur, op, ty, flags, result, ops, site);
        if cse && let Some(v) = result {
            self.cse.insert(Key::of(&self.key), (self.cur, v));
        }
        result
    }

    /// Emits an operation whose form always defines a value.
    fn value(&mut self, op: Op, ty: Ty, flags: u32, ops: &[u32]) -> Val {
        self.emit(op, ty, flags, ops).unwrap_or(Val::NONE)
    }

    /// A unary operation of type `ty`, or a string header operation on a `str16`.
    #[track_caller]
    pub fn un(&mut self, op: Op, a: Val) -> Val {
        let ty = self.ty(a);
        debug_assert_eq!(op.form(), Form::Un, "{} is not unary", op.name());
        if self.fold
            && let Some(x) = self.bits(a)
            && let Ok(r) = eval::unary(op, ty, op.result(ty), x)
        {
            return self.konst(op.result(ty), r);
        }
        self.value(op, ty, 0, &[a.0])
    }

    /// A binary operation, including `crc32c`. The type is the operands'.
    #[track_caller]
    pub fn bin(&mut self, op: Op, a: Val, b: Val) -> Val {
        let ty = self.ty(a);
        debug_assert!(matches!(op.form(), Form::Bin | Form::Cmp | Form::Wide | Form::StrMk));
        let (a, b) = if commutes(op) && self.bits(a).is_some() && self.bits(b).is_none() {
            (b, a)
        } else {
            (a, b)
        };
        if self.fold {
            let rty = op.result(ty);
            match (self.bits(a), self.bits(b)) {
                (Some(x), Some(y)) => {
                    if let Ok(r) = eval::binary(op, ty, x, y) {
                        return self.konst(rty, r);
                    }
                }
                (_, Some(y)) => {
                    if let Some(v) = self.identity(op, ty, a, y) {
                        return v;
                    }
                }
                _ => {}
            }
            if a == b {
                match op {
                    Op::And | Op::Or => return a,
                    Op::Sub | Op::Xor if ty.is_intlike() => return self.konst(ty, 0),
                    Op::IcmpEq | Op::IcmpSle | Op::IcmpSge | Op::IcmpUle | Op::IcmpUge => {
                        return self.bool(true);
                    }
                    Op::IcmpNe | Op::IcmpSlt | Op::IcmpSgt | Op::IcmpUlt | Op::IcmpUgt => {
                        return self.bool(false);
                    }
                    _ => {}
                }
            }
        }
        let ty = if op.form() == Form::StrMk { Ty::Str16 } else { ty };
        self.value(op, ty, 0, &[a.0, b.0])
    }

    /// The identities that fire when the right operand is the constant `y`.
    fn identity(&mut self, op: Op, ty: Ty, a: Val, y: u128) -> Option<Val> {
        let m = ty.mask();
        match op {
            Op::Add
            | Op::Sub
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
                if y == 0 =>
            {
                Some(a)
            }
            Op::Mul if y == 1 => Some(a),
            Op::Mul | Op::And if y == 0 => Some(self.konst(ty, 0)),
            Op::And if y == m => Some(a),
            Op::Or if y == m => Some(self.konst(ty, m)),
            _ => None,
        }
    }

    /// `select c, a, b`.
    #[track_caller]
    pub fn select(&mut self, c: Val, a: Val, b: Val) -> Val {
        let ty = self.ty(a);
        if self.fold {
            if let Some(x) = self.bits(c) {
                return if x != 0 { a } else { b };
            }
            if a == b {
                return a;
            }
            if ty == Ty::I1 && self.bits(a) == Some(1) && self.bits(b) == Some(0) {
                return c;
            }
        }
        self.value(Op::Select, ty, 0, &[c.0, a.0, b.0])
    }

    /// A conversion of `a` to type `to`.
    #[track_caller]
    pub fn conv(&mut self, op: Op, a: Val, to: Ty) -> Val {
        let from = self.ty(a);
        if self.fold {
            if from == to && matches!(op, Op::Sext | Op::Zext | Op::Trunc | Op::Bitcast) {
                return a;
            }
            if let Some(x) = self.bits(a)
                && let Ok(r) = eval::unary(op, from, to, x)
            {
                return self.konst(to, r);
            }
        }
        self.value(op, to, 0, &[a.0])
    }

    /// `ftosi.t a -> to, !E`.
    #[track_caller]
    pub fn ftosi(&mut self, a: Val, to: Ty, err: u32) -> Val {
        let from = self.ty(a);
        if self.fold
            && let Some(x) = self.bits(a)
            && let Ok(r) = eval::unary(Op::FtosiT, from, to, x)
        {
            return self.konst(to, r);
        }
        self.value(Op::FtosiT, to, 0, &[a.0, err])
    }

    /// Checked arithmetic in its trap form: `sadd.t a, b, !E` and the rest.
    #[track_caller]
    pub fn checked(&mut self, op: Op, a: Val, b: Val, err: u32) -> Val {
        let ty = self.ty(a);
        debug_assert_eq!(op.form(), Form::TrapBin);
        if self.fold {
            match (self.bits(a), self.bits(b)) {
                (Some(x), Some(y)) => {
                    if let Ok(r) = eval::binary(op, ty, x, y) {
                        return self.konst(ty, r);
                    }
                }
                (_, Some(0)) if matches!(op, Op::SaddT | Op::SsubT | Op::UaddT | Op::UsubT) => {
                    return a;
                }
                (_, Some(1)) if matches!(op, Op::SmulT | Op::UmulT | Op::SdivT | Op::UdivT) => {
                    return a;
                }
                _ => {}
            }
        }
        self.value(op, ty, 0, &[a.0, b.0, err])
    }

    /// `sneg.t a, !E`.
    #[track_caller]
    pub fn checked_neg(&mut self, a: Val, err: u32) -> Val {
        let ty = self.ty(a);
        if self.fold
            && let Some(x) = self.bits(a)
            && let Ok(r) = eval::unary(Op::SnegT, ty, ty, x)
        {
            return self.konst(ty, r);
        }
        self.value(Op::SnegT, ty, 0, &[a.0, err])
    }

    /// Checked arithmetic in its edge form. Ends the block: the result is the one parameter of
    /// `ok`, and `ovf` runs on overflow.
    #[track_caller]
    pub fn checked_edge(&mut self, op: Op, a: Val, b: Val, ok: Block, ovf: Block) {
        let ty = self.ty(a);
        self.emit(op, ty, 0, &[a.0, b.0, ok.0, ovf.0]);
    }

    /// `ddown a, k`.
    #[track_caller]
    pub fn ddown(&mut self, a: Val, k: u32) -> Val {
        let ty = self.ty(a);
        if self.fold {
            if k == 0 {
                return a;
            }
            if let Some(x) = self.bits(a)
                && let Ok(r) = eval::scale(Op::Ddown, ty, x, k)
            {
                return self.konst(ty, r);
            }
        }
        self.value(Op::Ddown, ty, 0, &[a.0, k])
    }

    /// `dup.t a, k, !E`.
    #[track_caller]
    pub fn dup(&mut self, a: Val, k: u32, err: u32) -> Val {
        let ty = self.ty(a);
        if self.fold {
            if k == 0 {
                return a;
            }
            if let Some(x) = self.bits(a)
                && let Ok(r) = eval::scale(Op::DupT, ty, x, k)
            {
                return self.konst(ty, r);
            }
        }
        self.value(Op::DupT, ty, 0, &[a.0, k, err])
    }

    // Memory.

    /// `load.ty [base + idx*scale + disp]`. `idx` may be [`Val::NONE`]. `flags` takes [`NT`],
    /// [`A16`] and [`INV`].
    #[track_caller]
    pub fn load(&mut self, ty: Ty, base: Val, idx: Val, scale: u32, disp: i32, flags: u32) -> Val {
        let op = if ty == Ty::Str16 { Op::LoadStr } else { Op::Load };
        let (idx, disp) = self.fold_index(idx, scale, disp);
        self.value(op, ty, flags & (NT | A16 | INV), &[base.0, idx.0, scale, disp as u32])
    }

    /// `store.ty [base + idx*scale + disp], v`.
    #[track_caller]
    pub fn store(&mut self, base: Val, idx: Val, scale: u32, disp: i32, v: Val, flags: u32) {
        let ty = self.ty(v);
        let op = if ty == Ty::Str16 { Op::StoreStr } else { Op::Store };
        let (idx, disp) = self.fold_index(idx, scale, disp);
        self.emit(op, ty, flags & (NT | A16), &[base.0, idx.0, scale, disp as u32, v.0]);
    }

    /// Moves a constant index into the displacement when it fits.
    fn fold_index(&mut self, idx: Val, scale: u32, disp: i32) -> (Val, i32) {
        if !self.fold || idx == Val::NONE {
            return (idx, disp);
        }
        match self.f.constant(idx) {
            Some(c) => {
                let off = i64::from(disp) + eval::sext(c.ty, c.bits) as i64 * i64::from(scale);
                match i32::try_from(off) {
                    Ok(d) => (Val::NONE, d),
                    Err(_) => (idx, disp),
                }
            }
            None => (idx, disp),
        }
    }

    /// `load.bit [base + idx]`, bit `idx` of the bitmap at `base`.
    #[track_caller]
    pub fn load_bit(&mut self, base: Val, idx: Val) -> Val {
        self.value(Op::LoadBit, Ty::I1, 0, &[base.0, idx.0])
    }

    /// `memcpy dst, src, n`.
    #[track_caller]
    pub fn memcpy(&mut self, dst: Val, src: Val, n: u32) {
        self.emit(Op::Memcpy, Ty::Void, 0, &[dst.0, src.0, n]);
    }

    /// `memeq a, b, n`.
    #[track_caller]
    pub fn memeq(&mut self, a: Val, b: Val, n: u32) -> Val {
        self.value(Op::Memeq, Ty::I1, 0, &[a.0, b.0, n])
    }

    /// `prefetch.r` or `prefetch.w` of `[base + idx*scale + disp]`.
    #[track_caller]
    pub fn prefetch(&mut self, write: bool, base: Val, idx: Val, scale: u32, disp: i32) {
        let op = if write { Op::PrefetchW } else { Op::PrefetchR };
        self.emit(op, Ty::Void, 0, &[base.0, idx.0, scale, disp as u32]);
    }

    /// `cas ty [addr], old, new`, true when the swap happened.
    #[track_caller]
    pub fn cas(&mut self, addr: Val, old: Val, new: Val) -> Val {
        let ty = self.ty(old);
        self.value(Op::Cas, ty, 0, &[addr.0, old.0, new.0])
    }

    /// `atomic.add ty [addr], v`, the value before the add.
    #[track_caller]
    pub fn atomic_add(&mut self, addr: Val, v: Val) -> Val {
        let ty = self.ty(v);
        self.value(Op::AtomicAdd, ty, 0, &[addr.0, v.0])
    }

    // Control.

    /// `br b(args)`.
    #[track_caller]
    pub fn br(&mut self, b: Block, args: &[Val]) {
        let mut ops = vec![b.0];
        ops.extend(args.iter().map(|v| v.0));
        self.emit(Op::Br, Ty::Void, 0, &ops);
    }

    /// `brif c, t(targs), f(fargs)`. A constant condition becomes a `br`.
    #[track_caller]
    pub fn brif(&mut self, c: Val, t: Block, targs: &[Val], f: Block, fargs: &[Val]) {
        if self.fold
            && let Some(x) = self.bits(c)
        {
            return if x != 0 { self.br(t, targs) } else { self.br(f, fargs) };
        }
        let mut ops = vec![c.0, t.0, targs.len() as u32];
        ops.extend(targs.iter().map(|v| v.0));
        ops.push(f.0);
        ops.extend(fargs.iter().map(|v| v.0));
        self.emit(Op::Brif, Ty::Void, 0, &ops);
    }

    /// `switch x, default(args), [k: b, ...]`. Case blocks take no arguments.
    #[track_caller]
    pub fn switch(&mut self, x: Val, default: Block, args: &[Val], cases: &[(u32, Block)]) {
        let ty = self.ty(x);
        let mut ops = vec![x.0, default.0, args.len() as u32];
        ops.extend(args.iter().map(|v| v.0));
        for (k, b) in cases {
            ops.push(*k);
            ops.push(b.0);
        }
        self.emit(Op::Switch, ty, 0, &ops);
    }

    /// `ret status`.
    #[track_caller]
    pub fn ret(&mut self, status: Val) {
        self.emit(Op::Ret, Ty::I64, 0, &[status.0]);
    }

    /// `trap !E`.
    #[track_caller]
    pub fn trap(&mut self, err: u32) {
        self.emit(Op::Trap, Ty::Void, 0, &[err]);
    }

    /// `rtcall @proxy(args)`, with the result when the proxy has one.
    #[track_caller]
    pub fn rtcall(&mut self, proxy: u32, args: &[Val]) -> Option<Val> {
        let p = crate::ir::CATALOGUE[proxy as usize];
        let mut ops = vec![proxy];
        ops.extend(args.iter().map(|v| v.0));
        if self.fold && p.pure && !p.mayfail {
            let words: Vec<u32> = std::iter::once(header(Op::Rtcall, p.ret, 0, ops.len()))
                .chain(ops.iter().copied())
                .collect();
            let key = Key::of(&words);
            if let Some(&(b, v)) = self.cse.get(&key)
                && (b == self.cur || b == Block(0))
            {
                return Some(v);
            }
            let r = self.emit(Op::Rtcall, p.ret, 0, &ops);
            if let Some(v) = r {
                self.cse.insert(key, (self.cur, v));
            }
            return r;
        }
        self.emit(Op::Rtcall, p.ret, 0, &ops)
    }

    /// `vcall @kernel(n, buffers)`.
    #[track_caller]
    pub fn vcall(&mut self, kernel: u32, n: Val, buffers: &[Val]) {
        let mut ops = vec![kernel, n.0];
        ops.extend(buffers.iter().map(|v| v.0));
        self.emit(Op::Vcall, Ty::Void, 0, &ops);
    }

    /// `guard c, !G`. A guard on the constant `true` is dropped.
    #[track_caller]
    pub fn guard(&mut self, c: Val, guard: u32) {
        if self.fold && self.bits(c) == Some(1) {
            return;
        }
        self.emit(Op::Guard, Ty::Void, 0, &[c.0, guard]);
    }

    /// `poll n`.
    #[track_caller]
    pub fn poll(&mut self, n: u32) {
        self.emit(Op::Poll, Ty::Void, 0, &[n]);
    }

    /// `ctr.add #k, v`.
    #[track_caller]
    pub fn ctr_add(&mut self, counter: u32, v: Val) {
        if self.fold && self.bits(v) == Some(0) {
            return;
        }
        self.emit(Op::CtrAdd, Ty::Void, 0, &[counter, v.0]);
    }
}

fn commutes(op: Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Mul
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::IcmpEq
            | Op::IcmpNe
            | Op::Fadd
            | Op::Fmul
            | Op::FcmpEq
    )
}

/// Marks instructions whose results nothing reads as removed, in one backward pass per block
/// over a use count. Section 6.11. Returns how many it removed.
pub fn dce(f: &mut Func) -> usize {
    let mut uses = vec![0u32; f.vals.len()];
    for b in 0..f.blocks.len() {
        for i in f.insts(Block(b as u32)) {
            i.uses(|v| {
                if !v.is_const() && v != Val::NONE {
                    uses[v.index()] += 1;
                }
            });
        }
    }
    let mut removed = 0;
    let mut dead = Vec::new();
    let mut starts = Vec::new();
    // Removing one instruction can free its operands, so sweep until nothing changes. Each sweep
    // walks blocks backwards, which catches a chain inside one block in a single sweep.
    loop {
        let mut changed = false;
        for b in (0..f.blocks.len()).rev() {
            dead.clear();
            starts.clear();
            starts.extend(f.insts(Block(b as u32)).map(|inst| inst.at));
            for &at in starts.iter().rev() {
                let Some(inst) = f.inst_at(Block(b as u32), at) else { continue };
                if inst.dead() {
                    continue;
                }
                let Some(r) = inst.result else { continue };
                let op = inst.op;
                let removable =
                    op.is_pure() || matches!(op, Op::Load | Op::LoadBit | Op::LoadStr | Op::Memeq);
                if removable && uses[r.index()] == 0 {
                    dead.push(inst.at);
                    inst.uses(|v| {
                        if !v.is_const() && v != Val::NONE {
                            uses[v.index()] -= 1;
                        }
                    });
                }
            }
            for &at in &dead {
                f.blocks[b].code[at as usize] |= DEAD << 13;
                removed += 1;
                changed = true;
            }
        }
        if !changed {
            return removed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_fold_and_dedup() {
        let mut b = Builder::new("f", "generic", 0);
        let two = b.int(Ty::I32, 2);
        let three = b.int(Ty::I32, 3);
        let five = b.bin(Op::Add, two, three);
        assert_eq!(b.func().constant(five).unwrap().bits, 5);
        assert_eq!(b.int(Ty::I32, 5), five);
        assert_eq!(b.func().count(), 0);
    }

    #[test]
    fn nullable_identities_disappear() {
        let mut b = Builder::new("f", "generic", 0);
        let m = b.m();
        let x = b.load(Ty::I1, m, Val::NONE, 1, 0, 0);
        let t = b.bool(true);
        let f = b.bool(false);
        assert_eq!(b.bin(Op::And, x, t), x);
        assert_eq!(b.bin(Op::Or, x, f), x);
        assert_eq!(b.select(t, x, f), x);
        assert_eq!(b.select(x, t, f), x);
    }

    #[test]
    fn pure_ops_and_invariant_loads_are_shared() {
        let mut b = Builder::new("f", "generic", 0);
        let st = b.st();
        let a = b.load(Ty::I64, st, Val::NONE, 1, 8, INV);
        let a2 = b.load(Ty::I64, st, Val::NONE, 1, 8, INV);
        assert_eq!(a, a2);
        let c = b.load(Ty::I64, st, Val::NONE, 1, 16, 0);
        let c2 = b.load(Ty::I64, st, Val::NONE, 1, 16, 0);
        assert_ne!(c, c2);
        let s = b.bin(Op::Add, a, c);
        assert_eq!(b.bin(Op::Add, a, c), s);
        let next = b.block(&[]);
        b.br(next, &[]);
        b.switch_to(next);
        // The entry block dominates everything, so a hit there is still good.
        assert_eq!(b.bin(Op::Add, a, c), s);
    }

    #[test]
    fn provenance_names_the_caller() {
        let mut b = Builder::new("f", "generic", 7);
        let m = b.m();
        let _ = b.load(Ty::I64, m, Val::NONE, 1, 0, 0);
        let f = b.finish();
        let site = f.sites[f.blocks[0].prov[0] as usize];
        assert_eq!(site.plan, 7);
        assert!(site.file.ends_with("build.rs"), "{}", site.file);
    }

    #[test]
    fn dce_removes_unused_chains_and_keeps_effects() {
        let mut b = Builder::new("f", "generic", 0);
        let st = b.st();
        let x = b.load(Ty::I64, st, Val::NONE, 1, 0, 0);
        let y = b.bin(Op::Mul, x, x);
        let _unused = b.bin(Op::Add, y, x);
        b.store(st, Val::NONE, 1, 8, x, 0);
        let zero = b.int(Ty::I64, 0);
        b.ret(zero);
        let mut f = b.finish();
        assert_eq!(dce(&mut f), 2);
        assert_eq!(f.count(), 3);
    }
}
