// Lifted from rudb crates/rudb-qc-ir/src/func.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! Functions, blocks, values and the module tables, per `spec/compiler/06-qir.md` sections 6.2 and
//! 6.9.
//!
//! A block's instructions are one `Vec<u32>`. An instruction is a header word, then its result
//! value if it has one, then its operand words as its [`Form`] lays them out. The header is the
//! opcode in 8 bits, the header type in 5, flags in 5 and the number of words that follow in 14.
//! Section 6.2 gives the count 6 bits and keeps 8 reserved, and that turned out to be too few for
//! a `switch` or a `brif` carrying a loop's state, so the reserved bits went to the count.

use crate::ir::{Class, Form, Op, Ty};

/// A value: a dense value number, or, with the top bit set, an index into the constant pool.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Val(pub u32);

impl Val {
    const CONST: u32 = 1 << 31;

    /// The absent index of a load or store with no index register.
    pub const NONE: Val = Val(u32::MAX);

    /// The constant at `index` in the pool.
    #[must_use]
    pub fn konst(index: u32) -> Val {
        Val(index | Self::CONST)
    }

    /// Whether this names a pool entry and not an instruction result.
    #[must_use]
    pub fn is_const(self) -> bool {
        self.0 & Self::CONST != 0 && self != Val::NONE
    }

    /// The pool index of a constant.
    #[must_use]
    pub fn const_index(self) -> usize {
        (self.0 & !Self::CONST) as usize
    }

    /// The value number of a value that is not a constant.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A block, by its index in [`Func::blocks`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Block(pub u32);

impl Block {
    /// The index into [`Func::blocks`].
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Header flag: the address is known not to alias any store in the function.
pub const NT: u32 = 1;
/// Header flag: the address is 16 byte aligned.
pub const A16: u32 = 2;
/// Header flag: the load reads memory that does not change during one invocation, which is
/// `%st` and the morsel header. Such a load may be CSEd and hoisted.
pub const INV: u32 = 4;
/// Header flag: dead code elimination removed this instruction and every consumer skips it.
pub const DEAD: u32 = 8;

/// Packs a header word.
#[must_use]
pub fn header(op: Op, ty: Ty, flags: u32, words: usize) -> u32 {
    debug_assert!(words < (1 << 14), "an instruction of {words} words does not fit the header");
    (op as u32) | ((ty as u32) << 8) | ((flags & 31) << 13) | ((words as u32) << 18)
}

/// One instruction, read out of a block's arena.
#[derive(Clone, Copy, Debug)]
pub struct Inst<'a> {
    /// The offset of the header word in the block's code.
    pub at: u32,
    /// The opcode.
    pub op: Op,
    /// The header type, which is what the text form prints after the mnemonic.
    pub ty: Ty,
    /// The header flags.
    pub flags: u32,
    /// The value the instruction defines, if it defines one.
    pub result: Option<Val>,
    /// The operand words, laid out by the opcode's [`Form`].
    pub ops: &'a [u32],
}

impl Inst<'_> {
    /// Whether dead code elimination removed this.
    #[must_use]
    pub fn dead(&self) -> bool {
        self.flags & DEAD != 0
    }

    /// The operand word at `i` read as a value.
    #[must_use]
    pub fn val(&self, i: usize) -> Val {
        Val(self.ops[i])
    }

    /// Every value this instruction reads, in operand order, block arguments included.
    pub fn uses(&self, mut f: impl FnMut(Val)) {
        let o = self.ops;
        let v = |w: u32| Val(w);
        match self.op.form() {
            Form::Un | Form::Conv | Form::Ret => f(v(o[0])),
            Form::Bin | Form::Cmp | Form::Wide | Form::StrMk | Form::Atomic => {
                f(v(o[0]));
                f(v(o[1]));
            }
            Form::Sel | Form::Cas => {
                f(v(o[0]));
                f(v(o[1]));
                f(v(o[2]));
            }
            Form::TrapBin | Form::EdgeBin | Form::Memcpy | Form::Memeq | Form::LoadBit => {
                f(v(o[0]));
                f(v(o[1]));
            }
            Form::TrapUn | Form::TrapConv | Form::Scale | Form::TrapScale | Form::Guard => {
                f(v(o[0]))
            }
            Form::Load | Form::Prefetch => {
                f(v(o[0]));
                if o[1] != Val::NONE.0 {
                    f(v(o[1]));
                }
            }
            Form::Store => {
                f(v(o[0]));
                if o[1] != Val::NONE.0 {
                    f(v(o[1]));
                }
                f(v(o[4]));
            }
            Form::Br => o[1..].iter().for_each(|w| f(v(*w))),
            Form::Brif => {
                f(v(o[0]));
                let n = o[2] as usize;
                o[3..3 + n].iter().for_each(|w| f(v(*w)));
                o[4 + n..].iter().for_each(|w| f(v(*w)));
            }
            Form::Switch => {
                f(v(o[0]));
                let n = o[2] as usize;
                o[3..3 + n].iter().for_each(|w| f(v(*w)));
            }
            Form::Rtcall => o[1..].iter().for_each(|w| f(v(*w))),
            Form::Vcall => o[1..].iter().for_each(|w| f(v(*w))),
            Form::CtrAdd => f(v(o[1])),
            Form::Trap | Form::Poll => {}
        }
    }

    /// Every successor of a terminator, with the argument words passed to it.
    pub fn succs(&self, mut f: impl FnMut(Block, &[u32])) {
        let o = self.ops;
        match self.op.form() {
            Form::Br => f(Block(o[0]), &o[1..]),
            Form::Brif => {
                let n = o[2] as usize;
                f(Block(o[1]), &o[3..3 + n]);
                f(Block(o[3 + n]), &o[4 + n..]);
            }
            Form::Switch => {
                let n = o[2] as usize;
                f(Block(o[1]), &o[3..3 + n]);
                for pair in o[3 + n..].as_chunks::<2>().0 {
                    f(Block(pair[1]), &[]);
                }
            }
            Form::EdgeBin => {
                f(Block(o[2]), &[]);
                f(Block(o[3]), &[]);
            }
            _ => {}
        }
    }
}

/// A block: its parameters, its code and what the builder declared about it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct BlockData {
    /// The block's parameters, each a value defined at the head of the block.
    pub params: Vec<Val>,
    /// The instruction words.
    pub code: Vec<u32>,
    /// One provenance id per instruction, in order, indexing [`Func::sites`].
    pub prov: Vec<u32>,
    /// The target of at least one back edge. Declared by the builder and checked by rule V4.
    pub is_loop: bool,
    /// The loop nesting depth of a loop header.
    pub depth: u8,
    /// A loop with a constant trip count, which rule V11 lets go without a `poll`.
    pub bounded: bool,
    /// The morsel batch loop, which the scheduler polls between morsels, so rule V11 skips it.
    pub batch: bool,
    /// Rarely taken: trap stubs, deopt exits, NULL slow paths. Laid out after every hot block.
    pub cold: bool,
}

/// What a value is, beyond its number.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ValInfo {
    /// The value's type.
    pub ty: Ty,
    /// For a `str16`, where its bytes live.
    pub class: Class,
    /// The name the builder was given, for the printer.
    pub name: Option<Box<str>>,
}

/// A pool entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Const {
    /// The constant's type.
    pub ty: Ty,
    /// Its bits, masked to its width. Floats are their IEEE bits.
    pub bits: u128,
}

/// A field of the pipeline's state, for rule V6.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Field {
    /// Byte offset from `%st`.
    pub offset: u32,
    /// Size in bytes.
    pub size: u32,
    /// What the field is, for the printer.
    pub name: String,
}

/// Where an instruction came from: the plan node, and the generator line that appended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Site {
    /// The plan node id.
    pub plan: u32,
    /// The generator's source file, or empty for a module that was parsed.
    pub file: &'static str,
    /// The generator's source line.
    pub line: u32,
}

/// One pipeline function: `fn(state: ptr, morsel: ptr) -> i64 status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Func {
    /// The function's name, `query.pipeline.version` by convention.
    pub name: String,
    /// Which specialization this is, `generic` when there is only one.
    pub version: String,
    /// The plan node the pipeline was cut from.
    pub plan: u32,
    /// The blocks. Block 0 is the entry and has the parameters `(ptr %st, ptr %m)`.
    pub blocks: Vec<BlockData>,
    /// One entry per value number.
    pub vals: Vec<ValInfo>,
    /// The constant pool.
    pub consts: Vec<Const>,
    /// The state layout the function may touch through `%st`. Empty means it was not declared and
    /// rule V6 has nothing to check.
    pub state: Vec<Field>,
    /// The pipeline's sink keeps everything it writes local to the morsel until the morsel ends,
    /// so a guard after an effect is allowed (rule V9).
    pub morsel_local: bool,
    /// Provenance sites. Entry 0 is the unknown site.
    pub sites: Vec<Site>,
    /// A value's validity, where the value has one that is not the constant `true`. Rule V12
    /// reads it.
    pub validity: Vec<(Val, Val)>,
}

impl Func {
    /// The type of a value or constant.
    #[must_use]
    pub fn ty(&self, v: Val) -> Ty {
        if v.is_const() { self.consts[v.const_index()].ty } else { self.vals[v.index()].ty }
    }

    /// The constant a value names, if it names one.
    #[must_use]
    pub fn constant(&self, v: Val) -> Option<Const> {
        v.is_const().then(|| self.consts[v.const_index()])
    }

    /// The instructions of a block, in order, with the removed ones included.
    #[must_use]
    pub fn insts(&self, b: Block) -> Insts<'_> {
        Insts { code: &self.blocks[b.index()].code, at: 0 }
    }

    /// The instruction whose header is at word `at` of block `b`.
    pub(crate) fn inst_at(&self, b: Block, at: u32) -> Option<Inst<'_>> {
        Insts { code: &self.blocks[b.index()].code, at: at as usize }.next()
    }

    /// The last instruction of a block, which a well formed block has as its terminator.
    #[must_use]
    pub fn terminator(&self, b: Block) -> Option<Inst<'_>> {
        self.insts(b).last()
    }

    /// The order the blocks are laid out in: every hot block in the order it was made, then every
    /// cold one. Section 6.2 principle 5. No backend reorders.
    #[must_use]
    pub fn layout(&self) -> Vec<Block> {
        let hot = (0..self.blocks.len()).filter(|&i| !self.blocks[i].cold);
        let cold = (0..self.blocks.len()).filter(|&i| self.blocks[i].cold);
        hot.chain(cold).map(|i| Block(i as u32)).collect()
    }

    /// How many instructions the function holds, removed ones not counted.
    #[must_use]
    pub fn count(&self) -> usize {
        (0..self.blocks.len())
            .map(|b| self.insts(Block(b as u32)).filter(|i| !i.dead()).count())
            .sum()
    }

    /// The validity recorded for a value.
    #[must_use]
    pub fn validity_of(&self, v: Val) -> Option<Val> {
        self.validity.iter().find(|(x, _)| *x == v).map(|(_, valid)| *valid)
    }
}

/// Walks a block's arena.
#[derive(Clone, Debug)]
pub struct Insts<'a> {
    code: &'a [u32],
    at: usize,
}

impl<'a> Iterator for Insts<'a> {
    type Item = Inst<'a>;

    fn next(&mut self) -> Option<Inst<'a>> {
        let head = *self.code.get(self.at)?;
        let op = Op::from_bits(head & 0xff).expect("a header with a known opcode");
        let ty = Ty::from_bits((head >> 8) & 31).expect("a header with a known type");
        let flags = (head >> 13) & 31;
        let words = (head >> 18) as usize;
        let body = &self.code[self.at + 1..self.at + 1 + words];
        let (result, ops) =
            if op.result(ty) == Ty::Void { (None, body) } else { (Some(Val(body[0])), &body[1..]) };
        let inst = Inst { at: self.at as u32, op, ty, flags, result, ops };
        self.at += 1 + words;
        Some(inst)
    }
}

/// What kind of failure an error site reports, which decides the message's first words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// `Out of Range Error: Overflow in ...`.
    Overflow,
    /// `Out of Range Error: Division by zero` and its relatives.
    DivideByZero,
    /// `Conversion Error: ...`.
    Conversion,
    /// `Out of Range Error: ...` other than an overflow.
    OutOfRange,
    /// The query was cancelled.
    Cancel,
    /// A failure the engine should never produce.
    Internal,
}

impl ErrorKind {
    /// The name the text form uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Overflow => "overflow",
            ErrorKind::DivideByZero => "divide",
            ErrorKind::Conversion => "conversion",
            ErrorKind::OutOfRange => "range",
            ErrorKind::Cancel => "cancel",
            ErrorKind::Internal => "internal",
        }
    }

    /// The kind the text form names this way.
    #[must_use]
    pub fn from_name(name: &str) -> Option<ErrorKind> {
        [
            ErrorKind::Overflow,
            ErrorKind::DivideByZero,
            ErrorKind::Conversion,
            ErrorKind::OutOfRange,
            ErrorKind::Cancel,
            ErrorKind::Internal,
        ]
        .into_iter()
        .find(|k| k.name() == name)
    }
}

/// An error site: the generated code carries its id and the runtime builds DuckDB's message
/// from it and the operands the error slot received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorSite {
    /// What failed.
    pub kind: ErrorKind,
    /// The rest of the message, for example `INT32 (a + b)`.
    pub text: String,
}

/// A guard: which fact it tests, and which version reruns the morsel when the fact is false.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardSite {
    /// The fact, as the physical planner names it.
    pub fact: String,
    /// The function that runs the morsel instead.
    pub fallback: String,
    /// The condition depends only on the morsel header and the state, so it can be hoisted.
    pub invariant: bool,
}

/// A counter in the pipeline's counter block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counter {
    /// What it counts.
    pub name: String,
}

/// A vectorized kernel of the first engine that a `vcall` reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kernel {
    /// The function name and argument types, which the runtime resolves in the first engine's
    /// registry.
    pub name: String,
}

/// A QIR module: the unit a backend compiles and the code cache stores.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Module {
    /// The module's name.
    pub name: String,
    /// One function per pipeline and version.
    pub funcs: Vec<Func>,
    /// Error sites, indexed by `!E`.
    pub errors: Vec<ErrorSite>,
    /// Guards, indexed by `!G`.
    pub guards: Vec<GuardSite>,
    /// Counters, indexed by `#k`.
    pub counters: Vec<Counter>,
    /// Kernels, indexed by the `vcall` operand.
    pub kernels: Vec<Kernel>,
    /// Constant blobs copied into query state at pipeline start: LIKE patterns, IN lists,
    /// dictionary bitmaps.
    pub blobs: Vec<Vec<u8>>,
}

impl Module {
    /// An empty module.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Module { name: name.to_owned(), ..Module::default() }
    }

    /// Adds an error site and returns its id.
    pub fn error(&mut self, kind: ErrorKind, text: &str) -> u32 {
        self.errors.push(ErrorSite { kind, text: text.to_owned() });
        (self.errors.len() - 1) as u32
    }

    /// Adds a guard and returns its id.
    pub fn guard(&mut self, fact: &str, fallback: &str, invariant: bool) -> u32 {
        self.guards.push(GuardSite {
            fact: fact.to_owned(),
            fallback: fallback.to_owned(),
            invariant,
        });
        (self.guards.len() - 1) as u32
    }

    /// Adds a counter and returns its id.
    pub fn counter(&mut self, name: &str) -> u32 {
        self.counters.push(Counter { name: name.to_owned() });
        (self.counters.len() - 1) as u32
    }

    /// Adds a kernel reference and returns its id.
    pub fn kernel(&mut self, name: &str) -> u32 {
        self.kernels.push(Kernel { name: name.to_owned() });
        (self.kernels.len() - 1) as u32
    }

    /// The function with this name.
    #[must_use]
    pub fn func(&self, name: &str) -> Option<&Func> {
        self.funcs.iter().find(|f| f.name == name)
    }
}
