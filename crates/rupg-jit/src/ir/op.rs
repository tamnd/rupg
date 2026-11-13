// Lifted from rudb crates/rudb-qc-ir/src/op.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! The opcodes of QIR and the shape of each one's operands, per `spec/compiler/06-qir.md`
//! sections 6.4 and 6.5.

/// How an instruction's operand words are laid out after its header and its result.
///
/// The layout is fixed per opcode. `V` is a value word, `blk` a block id, and the rest are
/// immediates. Every consumer of the arena (the printer, the parser, the verifier, the backends)
/// walks operands through this and not through its own table, so there is one description of
/// what a word means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// `[V]`.
    Un,
    /// `[V, V]`.
    Bin,
    /// `[V, V]` with an `i1` result; the header type is the operand type.
    Cmp,
    /// `[V c, V a, V b]`.
    Sel,
    /// `[V, V, !E]`, returns an error status on overflow or a bad divisor.
    TrapBin,
    /// `[V, !E]`.
    TrapUn,
    /// `[V, V, blk ok, blk ovf]`, a terminator whose result is the one parameter of `ok`.
    EdgeBin,
    /// `[V, V]` of the header type, with a result twice as wide.
    Wide,
    /// `[V, k]`, a decimal rescale by a constant power of ten.
    Scale,
    /// `[V, k, !E]`.
    TrapScale,
    /// `[V]` of any type, with the header type as the result.
    Conv,
    /// `[V, !E]`, a conversion that traps.
    TrapConv,
    /// `[V base, V idx or none, scale, disp]` with a result of the header type.
    Load,
    /// `[V base, V idx or none, scale, disp, V value]`.
    Store,
    /// `[V base, V idx]`, bit `idx` of the bitmap at `base`.
    LoadBit,
    /// `[V dst, V src, n]`.
    Memcpy,
    /// `[V a, V b, n]`.
    Memeq,
    /// `[V base, V idx or none, scale, disp]`.
    Prefetch,
    /// `[V addr, V old, V new]`.
    Cas,
    /// `[V addr, V value]`.
    Atomic,
    /// `[V w0, V w1]`.
    StrMk,
    /// `[blk, V args...]`.
    Br,
    /// `[V c, blk t, n, V args..., blk f, V args...]`.
    Brif,
    /// `[V x, blk default, n, V args..., (k, blk)...]`.
    Switch,
    /// `[V status]`.
    Ret,
    /// `[!E]`.
    Trap,
    /// `[@proxy, V args...]`.
    Rtcall,
    /// `[@kernel, V n, V buffers...]`.
    Vcall,
    /// `[V c, !G]`.
    Guard,
    /// `[n]`.
    Poll,
    /// `[#k, V value]`.
    CtrAdd,
}

macro_rules! ops {
    ($($variant:ident = $name:literal, $form:ident;)*) => {
        /// A QIR opcode.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        #[repr(u8)]
        pub enum Op {
            $(
                #[doc = concat!("`", $name, "`")]
                $variant,
            )*
        }

        const OPS: &[Op] = &[$(Op::$variant,)*];

        impl Op {
            /// The mnemonic the text form uses.
            #[must_use]
            pub fn name(self) -> &'static str {
                match self {
                    $(Op::$variant => $name,)*
                }
            }

            /// How the operands of this opcode are laid out.
            #[must_use]
            pub fn form(self) -> Form {
                match self {
                    $(Op::$variant => Form::$form,)*
                }
            }
        }
    };
}

ops! {
    Add = "add", Bin;
    Sub = "sub", Bin;
    Mul = "mul", Bin;
    And = "and", Bin;
    Or = "or", Bin;
    Xor = "xor", Bin;
    Shl = "shl", Bin;
    Lshr = "lshr", Bin;
    Ashr = "ashr", Bin;
    Rotl = "rotl", Bin;
    Rotr = "rotr", Bin;
    Neg = "neg", Un;
    Not = "not", Un;
    Clz = "clz", Un;
    Ctz = "ctz", Un;
    Popcnt = "popcnt", Un;
    Bswap = "bswap", Un;
    SaddT = "sadd.t", TrapBin;
    SsubT = "ssub.t", TrapBin;
    SmulT = "smul.t", TrapBin;
    UaddT = "uadd.t", TrapBin;
    UsubT = "usub.t", TrapBin;
    UmulT = "umul.t", TrapBin;
    SnegT = "sneg.t", TrapUn;
    SaddOv = "sadd.ov", EdgeBin;
    SsubOv = "ssub.ov", EdgeBin;
    SmulOv = "smul.ov", EdgeBin;
    Smulw = "smulw", Wide;
    Umulw = "umulw", Wide;
    SdivT = "sdiv.t", TrapBin;
    SremT = "srem.t", TrapBin;
    UdivT = "udiv.t", TrapBin;
    UremT = "urem.t", TrapBin;
    DupT = "dup.t", TrapScale;
    Ddown = "ddown", Scale;
    Sext = "sext", Conv;
    Zext = "zext", Conv;
    Trunc = "trunc", Conv;
    Sitof = "sitof", Conv;
    Uitof = "uitof", Conv;
    FtosiT = "ftosi.t", TrapConv;
    Fext = "fext", Conv;
    Ftrunc = "ftrunc", Conv;
    Bitcast = "bitcast", Conv;
    Fadd = "fadd", Bin;
    Fsub = "fsub", Bin;
    Fmul = "fmul", Bin;
    Fdiv = "fdiv", Bin;
    FminTot = "fmin.tot", Bin;
    FmaxTot = "fmax.tot", Bin;
    Fneg = "fneg", Un;
    Fabs = "fabs", Un;
    Fsqrt = "fsqrt", Un;
    IcmpEq = "icmp.eq", Cmp;
    IcmpNe = "icmp.ne", Cmp;
    IcmpSlt = "icmp.slt", Cmp;
    IcmpSle = "icmp.sle", Cmp;
    IcmpSgt = "icmp.sgt", Cmp;
    IcmpSge = "icmp.sge", Cmp;
    IcmpUlt = "icmp.ult", Cmp;
    IcmpUle = "icmp.ule", Cmp;
    IcmpUgt = "icmp.ugt", Cmp;
    IcmpUge = "icmp.uge", Cmp;
    FcmpEq = "fcmp.eq.tot", Cmp;
    FcmpLt = "fcmp.lt.tot", Cmp;
    FcmpLe = "fcmp.le.tot", Cmp;
    Select = "select", Sel;
    Crc32c = "crc32c", Bin;
    Load = "load", Load;
    Store = "store", Store;
    LoadBit = "load.bit", LoadBit;
    Memcpy = "memcpy", Memcpy;
    Memeq = "memeq", Memeq;
    PrefetchR = "prefetch.r", Prefetch;
    PrefetchW = "prefetch.w", Prefetch;
    Cas = "cas", Cas;
    AtomicAdd = "atomic.add", Atomic;
    StrLen = "str.len", Un;
    StrW0 = "str.w0", Un;
    StrW1 = "str.w1", Un;
    StrPtr = "str.ptr", Un;
    StrInl = "str.inl", Un;
    StrMk = "str.mk", StrMk;
    LoadStr = "load.str", Load;
    StoreStr = "store.str", Store;
    Br = "br", Br;
    Brif = "brif", Brif;
    Switch = "switch", Switch;
    Ret = "ret", Ret;
    Trap = "trap", Trap;
    Rtcall = "rtcall", Rtcall;
    Vcall = "vcall", Vcall;
    Guard = "guard", Guard;
    Poll = "poll", Poll;
    CtrAdd = "ctr.add", CtrAdd;
}

impl Op {
    /// The opcode with this encoding, if there is one.
    #[must_use]
    pub fn from_bits(bits: u32) -> Option<Op> {
        OPS.get(bits as usize).copied()
    }

    /// The opcode the text form names this way.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Op> {
        OPS.iter().copied().find(|o| o.name() == name)
    }

    /// Every opcode, in encoding order.
    #[must_use]
    pub fn all() -> &'static [Op] {
        OPS
    }

    /// Whether this ends a block.
    #[must_use]
    pub fn is_terminator(self) -> bool {
        matches!(
            self.form(),
            Form::Br | Form::Brif | Form::Switch | Form::Ret | Form::Trap | Form::EdgeBin
        )
    }

    /// Whether this can return an error status from the middle of a block.
    ///
    /// The trap forms, `guard`, `poll` and the calls. A block with fifty of them is still one
    /// block, which is the point of section 6.4.
    #[must_use]
    pub fn traps(self) -> bool {
        matches!(
            self.form(),
            Form::TrapBin
                | Form::TrapUn
                | Form::TrapScale
                | Form::TrapConv
                | Form::Guard
                | Form::Vcall
        ) || matches!(self, Op::Poll | Op::Rtcall)
    }

    /// Whether this writes memory or shared state, which is what rule V9 is about and what dead
    /// code elimination must keep.
    #[must_use]
    pub fn has_effect(self) -> bool {
        matches!(
            self,
            Op::Store
                | Op::StoreStr
                | Op::Memcpy
                | Op::Cas
                | Op::AtomicAdd
                | Op::Rtcall
                | Op::Vcall
                | Op::CtrAdd
                | Op::PrefetchR
                | Op::PrefetchW
        )
    }

    /// Whether the result depends only on the operands, so two copies can be one.
    ///
    /// Loads are not pure. The builder CSEs the ones flagged invariant separately.
    #[must_use]
    pub fn is_pure(self) -> bool {
        !self.is_terminator()
            && !self.traps()
            && !self.has_effect()
            && !matches!(self, Op::Load | Op::LoadBit | Op::LoadStr | Op::Memeq)
    }

    /// The result type of an instruction of this opcode whose header type is `ty`.
    ///
    /// `Void` for an instruction with no result. A call's header type is its result type, and an
    /// edge form's result is a parameter of its `ok` block, not the instruction's.
    #[must_use]
    pub fn result(self, ty: crate::ir::Ty) -> crate::ir::Ty {
        use crate::ir::Ty;
        match self.form() {
            Form::Cmp | Form::LoadBit | Form::Memeq | Form::Cas => Ty::I1,
            Form::Wide => Ty::I128,
            Form::StrMk => Ty::Str16,
            Form::Store
            | Form::Memcpy
            | Form::Prefetch
            | Form::Br
            | Form::Brif
            | Form::Switch
            | Form::Ret
            | Form::Trap
            | Form::EdgeBin
            | Form::Vcall
            | Form::Guard
            | Form::Poll
            | Form::CtrAdd => Ty::Void,
            _ => match self {
                Op::StrLen => Ty::I32,
                Op::StrW0 | Op::StrW1 => Ty::I64,
                Op::StrPtr => Ty::Ptr,
                Op::StrInl => Ty::I1,
                Op::LoadStr => Ty::Str16,
                Op::Crc32c => Ty::I64,
                _ => ty,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_opcode_round_trips_through_its_bits_and_its_name() {
        for (i, op) in Op::all().iter().enumerate() {
            assert_eq!(*op as usize, i);
            assert_eq!(Op::from_bits(i as u32), Some(*op));
            assert_eq!(Op::from_name(op.name()), Some(*op));
        }
    }

    #[test]
    fn the_opcode_space_fits_the_header() {
        assert!(Op::all().len() <= 256);
    }
}
