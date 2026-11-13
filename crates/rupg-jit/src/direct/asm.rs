// Lifted from rudb crates/rudb-qc-direct/src/asm.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the tier are under `crate::direct` and QIR is `crate::ir`.

//! The x86-64 encoder: one method per instruction form the code generator emits, writing bytes
//! into a buffer that is reused from function to function.
//!
//! Every method takes registers and memory operands that are already chosen, and the encoding is
//! a few table lookups and pushes: a legacy prefix, REX, the opcode, ModRM, SIB, a displacement
//! and an immediate. Nothing here allocates apart from the buffer growing. A branch to a label
//! that is not placed yet is a `rel32` patched when the function is finished, and a branch back
//! to one that is placed uses `rel8` when it reaches.
//!
//! The tests decode every form with a disassembler and compare the text, for every register and
//! the memory operands whose encodings have special cases: `rsp` and `r12` as a base need a SIB
//! byte, and `rbp` and `r13` as a base need a displacement even when it is zero.

/// A general purpose register, by its hardware number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Reg(pub u8);

impl Reg {
    /// `rax`.
    pub const RAX: Reg = Reg(0);
    /// `rcx`.
    pub const RCX: Reg = Reg(1);
    /// `rdx`.
    pub const RDX: Reg = Reg(2);
    /// `rbx`.
    pub const RBX: Reg = Reg(3);
    /// `rsp`.
    pub const RSP: Reg = Reg(4);
    /// `rbp`.
    pub const RBP: Reg = Reg(5);
    /// `rsi`.
    pub const RSI: Reg = Reg(6);
    /// `rdi`.
    pub const RDI: Reg = Reg(7);
    /// `r8`.
    pub const R8: Reg = Reg(8);
    /// `r9`.
    pub const R9: Reg = Reg(9);
    /// `r10`.
    pub const R10: Reg = Reg(10);
    /// `r11`.
    pub const R11: Reg = Reg(11);
    /// `r12`.
    pub const R12: Reg = Reg(12);
    /// `r13`.
    pub const R13: Reg = Reg(13);
    /// `r14`.
    pub const R14: Reg = Reg(14);
    /// `r15`.
    pub const R15: Reg = Reg(15);
}

/// An SSE register, by its number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Xmm(pub u8);

/// An operand size.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Size {
    /// 8 bits.
    B,
    /// 16 bits.
    W,
    /// 32 bits.
    D,
    /// 64 bits.
    Q,
}

/// A float precision.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Prec {
    /// `f32`, the `ss` forms.
    S,
    /// `f64`, the `sd` forms.
    D,
}

impl Prec {
    fn prefix(self) -> u8 {
        match self {
            Prec::S => 0xf3,
            Prec::D => 0xf2,
        }
    }
}

/// A memory operand: `[base + index * scale + disp]`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Mem {
    /// The base register.
    pub base: Reg,
    /// The index register and the scale, 1, 2, 4 or 8. `rsp` cannot be an index.
    pub index: Option<(Reg, u8)>,
    /// The displacement.
    pub disp: i32,
}

impl Mem {
    /// `[base + disp]`.
    #[must_use]
    pub fn at(base: Reg, disp: i32) -> Mem {
        Mem { base, index: None, disp }
    }

    /// `[base + index * scale + disp]`.
    #[must_use]
    pub fn indexed(base: Reg, index: Reg, scale: u8, disp: i32) -> Mem {
        debug_assert!(index != Reg::RSP, "rsp cannot be an index");
        debug_assert!(matches!(scale, 1 | 2 | 4 | 8), "a scale of {scale}");
        Mem { base, index: Some((index, scale)), disp }
    }
}

/// A condition code, numbered as the `jcc`, `setcc` and `cmovcc` opcodes number them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Cc {
    /// Overflow.
    O = 0,
    /// No overflow.
    No = 1,
    /// Unsigned below, carry.
    B = 2,
    /// Unsigned above or equal, no carry.
    Ae = 3,
    /// Equal, zero.
    E = 4,
    /// Not equal, not zero.
    Ne = 5,
    /// Unsigned below or equal.
    Be = 6,
    /// Unsigned above.
    A = 7,
    /// Sign.
    S = 8,
    /// No sign.
    Ns = 9,
    /// Parity, which after `ucomisd` means unordered.
    P = 10,
    /// No parity.
    Np = 11,
    /// Signed less.
    L = 12,
    /// Signed greater or equal.
    Ge = 13,
    /// Signed less or equal.
    Le = 14,
    /// Signed greater.
    G = 15,
}

impl Cc {
    const ALL: [Cc; 16] = [
        Cc::O,
        Cc::No,
        Cc::B,
        Cc::Ae,
        Cc::E,
        Cc::Ne,
        Cc::Be,
        Cc::A,
        Cc::S,
        Cc::Ns,
        Cc::P,
        Cc::Np,
        Cc::L,
        Cc::Ge,
        Cc::Le,
        Cc::G,
    ];

    /// The opposite condition.
    #[must_use]
    pub fn invert(self) -> Cc {
        Cc::ALL[(self as usize) ^ 1]
    }
}

/// The eight arithmetic operations of the `00`..`3f` opcode block, by their `/n` number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Alu {
    /// `add`.
    Add = 0,
    /// `or`.
    Or = 1,
    /// `adc`.
    Adc = 2,
    /// `sbb`.
    Sbb = 3,
    /// `and`.
    And = 4,
    /// `sub`.
    Sub = 5,
    /// `xor`.
    Xor = 6,
    /// `cmp`.
    Cmp = 7,
}

/// The one operand group of `f6` and `f7`, by `/n`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Unary {
    /// `not`.
    Not = 2,
    /// `neg`.
    Neg = 3,
    /// `mul`, unsigned, into `rdx:rax`.
    Mul = 4,
    /// `imul`, signed, into `rdx:rax`.
    Imul = 5,
    /// `div`, unsigned, of `rdx:rax`.
    Div = 6,
    /// `idiv`, signed, of `rdx:rax`.
    Idiv = 7,
}

/// The shifts and rotates of `c1` and `d3`, by `/n`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Shift {
    /// `rol`.
    Rol = 0,
    /// `ror`.
    Ror = 1,
    /// `shl`.
    Shl = 4,
    /// `shr`.
    Shr = 5,
    /// `sar`.
    Sar = 7,
}

/// The bit counting instructions.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Bits {
    /// `bsf`.
    Bsf,
    /// `bsr`.
    Bsr,
    /// `tzcnt`, BMI1.
    Tzcnt,
    /// `lzcnt`, ABM.
    Lzcnt,
    /// `popcnt`.
    Popcnt,
}

/// The scalar SSE arithmetic, by the opcode byte after `0f`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Fop {
    /// `sqrt`.
    Sqrt = 0x51,
    /// `add`.
    Add = 0x58,
    /// `mul`.
    Mul = 0x59,
    /// `sub`.
    Sub = 0x5c,
    /// `min`.
    Min = 0x5d,
    /// `div`.
    Div = 0x5e,
    /// `max`.
    Max = 0x5f,
}

/// A place in the code a branch can go to, placed once with [`Asm::bind`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Label(pub u32);

const UNBOUND: u32 = u32::MAX;

/// The r/m operand of an instruction.
#[derive(Clone, Copy)]
enum Rm {
    R(u8),
    M(Mem),
    /// `[rip + disp32]` to a label.
    Rip(Label),
}

/// The code buffer and its labels.
#[derive(Clone, Debug, Default)]
pub struct Asm {
    /// The bytes so far.
    pub code: Vec<u8>,
    labels: Vec<u32>,
    /// `rel32` fields to fill in: where the field is, and the label it points at. The field ends
    /// `tail` bytes before the end of the instruction, which is what the offset counts from.
    fixups: Vec<(u32, Label, u8)>,
}

fn fits8(x: i64) -> bool {
    i8::try_from(x).is_ok()
}

impl Asm {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Asm {
        Asm::default()
    }

    /// Empties the buffer for the next function, keeping its memory.
    pub fn clear(&mut self) {
        self.code.clear();
        self.labels.clear();
        self.fixups.clear();
    }

    /// How many bytes have been written.
    #[must_use]
    pub fn len(&self) -> usize {
        self.code.len()
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// A new label, not placed yet.
    pub fn label(&mut self) -> Label {
        self.labels.push(UNBOUND);
        Label(self.labels.len() as u32 - 1)
    }

    /// Places `l` here.
    pub fn bind(&mut self, l: Label) {
        debug_assert_eq!(self.labels[l.0 as usize], UNBOUND, "label {} placed twice", l.0);
        self.labels[l.0 as usize] = self.code.len() as u32;
    }

    /// Where `l` was placed, if it has been.
    #[must_use]
    pub fn offset(&self, l: Label) -> Option<u32> {
        let at = self.labels[l.0 as usize];
        (at != UNBOUND).then_some(at)
    }

    /// Fills in every `rel32` that points at a label.
    ///
    /// # Errors
    ///
    /// The label a branch goes to that was never placed.
    pub fn finish(&mut self) -> Result<(), Label> {
        for &(at, l, tail) in &self.fixups {
            let target = self.labels[l.0 as usize];
            if target == UNBOUND {
                return Err(l);
            }
            let end = i64::from(at) + 4 + i64::from(tail);
            let rel = (i64::from(target) - end) as i32;
            self.code[at as usize..at as usize + 4].copy_from_slice(&rel.to_le_bytes());
        }
        self.fixups.clear();
        Ok(())
    }

    /// Writes one byte.
    pub fn byte(&mut self, b: u8) {
        self.code.push(b);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.code.extend_from_slice(b);
    }

    fn imm32(&mut self, x: i32) {
        self.bytes(&x.to_le_bytes());
    }

    /// Overwrites four bytes at `at`, for a field whose value is only known later.
    pub fn patch32(&mut self, at: usize, x: u32) {
        self.code[at..at + 4].copy_from_slice(&x.to_le_bytes());
    }

    /// Pads with `nop`s to a multiple of `n` bytes.
    pub fn align(&mut self, n: usize) {
        while !self.code.len().is_multiple_of(n) {
            self.byte(0x90);
        }
    }

    /// The general encoder: an optional legacy prefix, REX when needed, the opcode, then ModRM
    /// and whatever the r/m operand needs. `reg` is a register number or a `/n` extension.
    /// `byte` asks for a REX prefix even with no bits set when a byte register numbered 4 to 7
    /// is used, so that it means `spl`..`dil` and not `ah`..`bh`; bit 0 is for `reg` and bit 1
    /// for the r/m register. `tail` is how many immediate bytes will follow.
    #[allow(clippy::too_many_arguments)]
    fn inst(&mut self, pfx: u8, w: bool, byte: u8, op: &[u8], reg: u8, rm: Rm, tail: u8) {
        if pfx != 0 {
            self.byte(pfx);
        }
        let (x, b, rm_reg) = match rm {
            Rm::R(r) => (0, r >> 3, Some(r)),
            Rm::M(m) => (m.index.map_or(0, |(i, _)| i.0 >> 3), m.base.0 >> 3, None),
            Rm::Rip(_) => (0, 0, None),
        };
        let rex = 0x40 | (u8::from(w) << 3) | ((reg >> 3) << 2) | (x << 1) | b;
        let force = (byte & 1 != 0 && (4..8).contains(&reg))
            || (byte & 2 != 0 && rm_reg.is_some_and(|r| (4..8).contains(&r)));
        if rex != 0x40 || force {
            self.byte(rex);
        }
        self.bytes(op);
        let reg = (reg & 7) << 3;
        match rm {
            Rm::R(r) => self.byte(0xc0 | reg | (r & 7)),
            Rm::Rip(l) => {
                self.byte(reg | 5);
                let at = self.code.len() as u32;
                self.fixups.push((at, l, tail));
                self.imm32(0);
            }
            Rm::M(m) => {
                let base = m.base.0 & 7;
                let d = i64::from(m.disp);
                let md = if m.disp == 0 && base != 5 {
                    0
                } else if fits8(d) {
                    1
                } else {
                    2
                };
                match m.index {
                    Some((i, s)) => {
                        let ss = match s {
                            1 => 0,
                            2 => 1,
                            4 => 2,
                            _ => 3,
                        };
                        self.byte((md << 6) | reg | 4);
                        self.byte((ss << 6) | ((i.0 & 7) << 3) | base);
                    }
                    None if base == 4 => {
                        self.byte((md << 6) | reg | 4);
                        self.byte(0x24);
                    }
                    None => self.byte((md << 6) | reg | base),
                }
                match md {
                    0 => {}
                    1 => self.byte(m.disp as u8),
                    _ => self.imm32(m.disp),
                }
            }
        }
    }

    /// An integer instruction of `size` whose opcode is `op8` at 8 bits and `op` at the others.
    #[allow(clippy::too_many_arguments)]
    fn sized(&mut self, size: Size, op8: &[u8], op: &[u8], reg: u8, rm: Rm, byte: u8, tail: u8) {
        match size {
            Size::B => self.inst(0, false, byte, op8, reg, rm, tail),
            Size::W => self.inst(0x66, false, 0, op, reg, rm, tail),
            Size::D => self.inst(0, false, 0, op, reg, rm, tail),
            Size::Q => self.inst(0, true, 0, op, reg, rm, tail),
        }
    }

    // Moves.

    /// `mov d, s`.
    pub fn mov(&mut self, size: Size, d: Reg, s: Reg) {
        self.sized(size, &[0x88], &[0x89], s.0, Rm::R(d.0), 3, 0);
    }

    /// Sets the 64 bits of `d` to `x`, in the shortest of the three encodings.
    pub fn mov_imm(&mut self, d: Reg, x: u64) {
        if let Ok(x) = u32::try_from(x) {
            if d.0 >= 8 {
                self.byte(0x41);
            }
            self.byte(0xb8 | (d.0 & 7));
            self.bytes(&x.to_le_bytes());
        } else if let Ok(x) = i32::try_from(x as i64) {
            self.inst(0, true, 0, &[0xc7], 0, Rm::R(d.0), 4);
            self.imm32(x);
        } else {
            self.byte(0x48 | (d.0 >> 3));
            self.byte(0xb8 | (d.0 & 7));
            self.bytes(&x.to_le_bytes());
        }
    }

    /// Loads `size` bytes at `m` into `d`, zero extended to 64 bits.
    pub fn load(&mut self, size: Size, d: Reg, m: Mem) {
        match size {
            Size::B => self.inst(0, false, 0, &[0x0f, 0xb6], d.0, Rm::M(m), 0),
            Size::W => self.inst(0, false, 0, &[0x0f, 0xb7], d.0, Rm::M(m), 0),
            Size::D => self.inst(0, false, 0, &[0x8b], d.0, Rm::M(m), 0),
            Size::Q => self.inst(0, true, 0, &[0x8b], d.0, Rm::M(m), 0),
        }
    }

    /// Loads `from` bytes at `m` into `d`, sign extended to `to`, which is `D` or `Q`.
    pub fn load_sx(&mut self, from: Size, to: Size, d: Reg, m: Mem) {
        let w = to == Size::Q;
        match from {
            Size::B => self.inst(0, w, 0, &[0x0f, 0xbe], d.0, Rm::M(m), 0),
            Size::W => self.inst(0, w, 0, &[0x0f, 0xbf], d.0, Rm::M(m), 0),
            Size::D => self.inst(0, true, 0, &[0x63], d.0, Rm::M(m), 0),
            Size::Q => self.inst(0, true, 0, &[0x8b], d.0, Rm::M(m), 0),
        }
    }

    /// Stores the low `size` bytes of `s` at `m`.
    pub fn store(&mut self, size: Size, m: Mem, s: Reg) {
        self.sized(size, &[0x88], &[0x89], s.0, Rm::M(m), 1, 0);
    }

    /// Stores `x` at `m`, sign extended from 32 bits when `size` is `Q`.
    pub fn store_imm(&mut self, size: Size, m: Mem, x: i32) {
        match size {
            Size::B => {
                self.inst(0, false, 0, &[0xc6], 0, Rm::M(m), 1);
                self.byte(x as u8);
            }
            Size::W => {
                self.inst(0x66, false, 0, &[0xc7], 0, Rm::M(m), 2);
                self.bytes(&(x as u16).to_le_bytes());
            }
            Size::D | Size::Q => {
                self.inst(0, size == Size::Q, 0, &[0xc7], 0, Rm::M(m), 4);
                self.imm32(x);
            }
        }
    }

    /// `movzx d, s`: the low `from` bits of `s`, `B` or `W`, zero extended to 64.
    pub fn movzx(&mut self, from: Size, d: Reg, s: Reg) {
        match from {
            Size::B => self.inst(0, false, 2, &[0x0f, 0xb6], d.0, Rm::R(s.0), 0),
            Size::W => self.inst(0, false, 0, &[0x0f, 0xb7], d.0, Rm::R(s.0), 0),
            Size::D => self.mov(Size::D, d, s),
            Size::Q => self.mov(Size::Q, d, s),
        }
    }

    /// `movsx d, s`: the low `from` bits of `s` sign extended to `to`, which is `D` or `Q`.
    pub fn movsx(&mut self, from: Size, to: Size, d: Reg, s: Reg) {
        let w = to == Size::Q;
        match from {
            Size::B => self.inst(0, w, 2, &[0x0f, 0xbe], d.0, Rm::R(s.0), 0),
            Size::W => self.inst(0, w, 0, &[0x0f, 0xbf], d.0, Rm::R(s.0), 0),
            Size::D => self.inst(0, true, 0, &[0x63], d.0, Rm::R(s.0), 0),
            Size::Q => self.mov(Size::Q, d, s),
        }
    }

    /// `lea d, m`.
    pub fn lea(&mut self, d: Reg, m: Mem) {
        self.inst(0, true, 0, &[0x8d], d.0, Rm::M(m), 0);
    }

    /// `xchg a, b`, 64 bits.
    pub fn xchg(&mut self, a: Reg, b: Reg) {
        self.inst(0, true, 0, &[0x87], a.0, Rm::R(b.0), 0);
    }

    // Integer arithmetic.

    /// `op d, s`.
    pub fn alu(&mut self, op: Alu, size: Size, d: Reg, s: Reg) {
        let o = (op as u8) << 3;
        self.sized(size, &[o], &[o | 1], s.0, Rm::R(d.0), 3, 0);
    }

    /// `op d, x`, with `x` sign extended to `size`.
    pub fn alu_imm(&mut self, op: Alu, size: Size, d: Reg, x: i32) {
        self.alu_imm_rm(op, size, Rm::R(d.0), x);
    }

    /// `op size ptr [m], x`.
    pub fn alu_mem_imm(&mut self, op: Alu, size: Size, m: Mem, x: i32) {
        self.alu_imm_rm(op, size, Rm::M(m), x);
    }

    fn alu_imm_rm(&mut self, op: Alu, size: Size, rm: Rm, x: i32) {
        let n = op as u8;
        match size {
            Size::B => {
                self.inst(0, false, 2, &[0x80], n, rm, 1);
                self.byte(x as u8);
            }
            _ if fits8(i64::from(x)) => {
                self.sized(size, &[0x80], &[0x83], n, rm, 2, 1);
                self.byte(x as u8);
            }
            Size::W => {
                self.sized(size, &[0x80], &[0x81], n, rm, 2, 2);
                self.bytes(&(x as u16).to_le_bytes());
            }
            _ => {
                self.sized(size, &[0x80], &[0x81], n, rm, 2, 4);
                self.imm32(x);
            }
        }
    }

    /// `op d, size ptr [m]`.
    pub fn alu_load(&mut self, op: Alu, size: Size, d: Reg, m: Mem) {
        let o = (op as u8) << 3;
        self.sized(size, &[o | 2], &[o | 3], d.0, Rm::M(m), 1, 0);
    }

    /// `op size ptr [m], s`.
    pub fn alu_store(&mut self, op: Alu, size: Size, m: Mem, s: Reg) {
        let o = (op as u8) << 3;
        self.sized(size, &[o], &[o | 1], s.0, Rm::M(m), 1, 0);
    }

    /// `test a, b`.
    pub fn test(&mut self, size: Size, a: Reg, b: Reg) {
        self.sized(size, &[0x84], &[0x85], b.0, Rm::R(a.0), 3, 0);
    }

    /// `test a, x`.
    pub fn test_imm(&mut self, size: Size, a: Reg, x: i32) {
        match size {
            Size::B => {
                self.inst(0, false, 2, &[0xf6], 0, Rm::R(a.0), 1);
                self.byte(x as u8);
            }
            Size::W => {
                self.inst(0x66, false, 0, &[0xf7], 0, Rm::R(a.0), 2);
                self.bytes(&(x as u16).to_le_bytes());
            }
            _ => {
                self.sized(size, &[0xf6], &[0xf7], 0, Rm::R(a.0), 0, 4);
                self.imm32(x);
            }
        }
    }

    /// `imul d, s`, at 16 bits or more.
    pub fn imul(&mut self, size: Size, d: Reg, s: Reg) {
        self.sized(size, &[0x0f, 0xaf], &[0x0f, 0xaf], d.0, Rm::R(s.0), 0, 0);
    }

    /// `imul d, s, x`, at 16 bits or more.
    pub fn imul_imm(&mut self, size: Size, d: Reg, s: Reg, x: i32) {
        if fits8(i64::from(x)) {
            self.sized(size, &[0x6b], &[0x6b], d.0, Rm::R(s.0), 0, 1);
            self.byte(x as u8);
        } else if size == Size::W {
            self.sized(size, &[0x69], &[0x69], d.0, Rm::R(s.0), 0, 2);
            self.bytes(&(x as u16).to_le_bytes());
        } else {
            self.sized(size, &[0x69], &[0x69], d.0, Rm::R(s.0), 0, 4);
            self.imm32(x);
        }
    }

    /// One of the `f7` group on `r`.
    pub fn unary(&mut self, op: Unary, size: Size, r: Reg) {
        self.sized(size, &[0xf6], &[0xf7], op as u8, Rm::R(r.0), 2, 0);
    }

    /// `cwd`, `cdq` or `cqo`: sign extends `ax`, `eax` or `rax` into the `dx` register of the
    /// same size, before a signed divide.
    pub fn sign_extend_ax(&mut self, size: Size) {
        match size {
            Size::W => self.bytes(&[0x66, 0x99]),
            Size::Q => self.bytes(&[0x48, 0x99]),
            _ => self.byte(0x99),
        }
    }

    /// A shift or rotate of `r` by `cl`.
    pub fn shift_cl(&mut self, op: Shift, size: Size, r: Reg) {
        self.sized(size, &[0xd2], &[0xd3], op as u8, Rm::R(r.0), 2, 0);
    }

    /// A shift or rotate of `r` by `n`.
    pub fn shift_imm(&mut self, op: Shift, size: Size, r: Reg, n: u8) {
        if n == 1 {
            self.sized(size, &[0xd0], &[0xd1], op as u8, Rm::R(r.0), 2, 0);
        } else {
            self.sized(size, &[0xc0], &[0xc1], op as u8, Rm::R(r.0), 2, 1);
            self.byte(n);
        }
    }

    /// `setcc r8`.
    pub fn setcc(&mut self, cc: Cc, r: Reg) {
        self.inst(0, false, 2, &[0x0f, 0x90 | cc as u8], 0, Rm::R(r.0), 0);
    }

    /// `cmovcc d, s`, at 32 or 64 bits.
    pub fn cmov(&mut self, cc: Cc, size: Size, d: Reg, s: Reg) {
        self.sized(size, &[], &[0x0f, 0x40 | cc as u8], d.0, Rm::R(s.0), 0, 0);
    }

    /// `bt qword ptr [m], idx`: bit `idx` of the bitmap at `m`, into the carry flag. The index
    /// is signed and reaches past the qword, which is what a validity bitmap wants.
    pub fn bt_mem(&mut self, m: Mem, idx: Reg) {
        self.inst(0, true, 0, &[0x0f, 0xa3], idx.0, Rm::M(m), 0);
    }

    /// `bt r, idx`, at 16 bits or more: bit `idx` of `r` modulo the width, into the carry flag.
    pub fn bt(&mut self, size: Size, r: Reg, idx: Reg) {
        self.sized(size, &[], &[0x0f, 0xa3], idx.0, Rm::R(r.0), 0, 0);
    }

    /// `bt r, n`.
    pub fn bt_imm(&mut self, size: Size, r: Reg, n: u8) {
        self.sized(size, &[], &[0x0f, 0xba], 4, Rm::R(r.0), 0, 1);
        self.byte(n);
    }

    /// A bit count of `s` into `d`, at 16 bits or more.
    pub fn bits(&mut self, op: Bits, size: Size, d: Reg, s: Reg) {
        let (pfx, op) = match op {
            Bits::Bsf => (0, 0xbc),
            Bits::Bsr => (0, 0xbd),
            Bits::Tzcnt => (0xf3, 0xbc),
            Bits::Lzcnt => (0xf3, 0xbd),
            Bits::Popcnt => (0xf3, 0xb8),
        };
        let rm = Rm::R(s.0);
        match (pfx, size) {
            (0, _) => self.sized(size, &[], &[0x0f, op], d.0, rm, 0, 0),
            (_, Size::W) => {
                self.byte(0x66);
                self.inst(pfx, false, 0, &[0x0f, op], d.0, rm, 0);
            }
            _ => self.inst(pfx, size == Size::Q, 0, &[0x0f, op], d.0, rm, 0),
        }
    }

    /// `bswap r`, at 32 or 64 bits.
    pub fn bswap(&mut self, size: Size, r: Reg) {
        let rex = 0x40 | (u8::from(size == Size::Q) << 3) | (r.0 >> 3);
        if rex != 0x40 {
            self.byte(rex);
        }
        self.bytes(&[0x0f, 0xc8 | (r.0 & 7)]);
    }

    /// `crc32 d, s` on 64 bits: one step of CRC-32C, SSE4.2.
    pub fn crc32(&mut self, d: Reg, s: Reg) {
        self.inst(0xf2, true, 0, &[0x0f, 0x38, 0xf1], d.0, Rm::R(s.0), 0);
    }

    // The stack and control flow.

    /// `push r`.
    pub fn push(&mut self, r: Reg) {
        if r.0 >= 8 {
            self.byte(0x41);
        }
        self.byte(0x50 | (r.0 & 7));
    }

    /// `pop r`.
    pub fn pop(&mut self, r: Reg) {
        if r.0 >= 8 {
            self.byte(0x41);
        }
        self.byte(0x58 | (r.0 & 7));
    }

    /// `ret`.
    pub fn ret(&mut self) {
        self.byte(0xc3);
    }

    /// `ud2`, for a place the code cannot reach.
    pub fn ud2(&mut self) {
        self.bytes(&[0x0f, 0x0b]);
    }

    /// `call qword ptr [rip + l]`: an indirect call through a literal table slot at `l`.
    pub fn call_rip(&mut self, l: Label) {
        self.inst(0, false, 0, &[0xff], 2, Rm::Rip(l), 0);
    }

    /// `jmp l`.
    pub fn jmp(&mut self, l: Label) {
        match self.offset(l) {
            Some(at) if fits8(i64::from(at) - (self.code.len() as i64 + 2)) => {
                let rel = i64::from(at) - (self.code.len() as i64 + 2);
                self.bytes(&[0xeb, rel as u8]);
            }
            _ => {
                self.byte(0xe9);
                self.rel32(l);
            }
        }
    }

    /// `jcc l`.
    pub fn jcc(&mut self, cc: Cc, l: Label) {
        match self.offset(l) {
            Some(at) if fits8(i64::from(at) - (self.code.len() as i64 + 2)) => {
                let rel = i64::from(at) - (self.code.len() as i64 + 2);
                self.bytes(&[0x70 | cc as u8, rel as u8]);
            }
            _ => {
                self.bytes(&[0x0f, 0x80 | cc as u8]);
                self.rel32(l);
            }
        }
    }

    fn rel32(&mut self, l: Label) {
        let at = self.code.len() as u32;
        self.fixups.push((at, l, 0));
        self.imm32(0);
    }

    // SSE.

    /// `movss` or `movsd` from memory.
    pub fn fload(&mut self, p: Prec, d: Xmm, m: Mem) {
        self.inst(p.prefix(), false, 0, &[0x0f, 0x10], d.0, Rm::M(m), 0);
    }

    /// `movss` or `movsd` to memory.
    pub fn fstore(&mut self, p: Prec, m: Mem, s: Xmm) {
        self.inst(p.prefix(), false, 0, &[0x0f, 0x11], s.0, Rm::M(m), 0);
    }

    /// `movups d, [m]`: sixteen bytes, unaligned.
    pub fn vload(&mut self, d: Xmm, m: Mem) {
        self.inst(0, false, 0, &[0x0f, 0x10], d.0, Rm::M(m), 0);
    }

    /// `movups [m], s`: sixteen bytes, unaligned.
    pub fn vstore(&mut self, m: Mem, s: Xmm) {
        self.inst(0, false, 0, &[0x0f, 0x11], s.0, Rm::M(m), 0);
    }

    /// `movaps d, s`, a whole register copy.
    pub fn fmov(&mut self, d: Xmm, s: Xmm) {
        self.inst(0, false, 0, &[0x0f, 0x28], d.0, Rm::R(s.0), 0);
    }

    /// `movq` or `movd` from a general register, the bits unchanged and the rest cleared.
    pub fn to_xmm(&mut self, p: Prec, d: Xmm, s: Reg) {
        self.inst(0x66, p == Prec::D, 0, &[0x0f, 0x6e], d.0, Rm::R(s.0), 0);
    }

    /// `movq` or `movd` to a general register, zero extended.
    pub fn from_xmm(&mut self, p: Prec, d: Reg, s: Xmm) {
        self.inst(0x66, p == Prec::D, 0, &[0x0f, 0x7e], s.0, Rm::R(d.0), 0);
    }

    /// Scalar arithmetic, `d = d op s`, or `d = sqrt s`.
    pub fn farith(&mut self, op: Fop, p: Prec, d: Xmm, s: Xmm) {
        self.inst(p.prefix(), false, 0, &[0x0f, op as u8], d.0, Rm::R(s.0), 0);
    }

    /// `ucomiss` or `ucomisd`: an unordered compare into the flags.
    pub fn ucomis(&mut self, p: Prec, a: Xmm, b: Xmm) {
        let pfx = if p == Prec::D { 0x66 } else { 0 };
        self.inst(pfx, false, 0, &[0x0f, 0x2e], a.0, Rm::R(b.0), 0);
    }

    /// `cvtsi2ss` or `cvtsi2sd` from a signed 64 bit integer.
    pub fn int_to_float(&mut self, p: Prec, d: Xmm, s: Reg) {
        self.inst(p.prefix(), true, 0, &[0x0f, 0x2a], d.0, Rm::R(s.0), 0);
    }

    /// `cvttss2si` or `cvttsd2si` to a signed 64 bit integer, truncating.
    pub fn float_to_int(&mut self, p: Prec, d: Reg, s: Xmm) {
        self.inst(p.prefix(), true, 0, &[0x0f, 0x2c], d.0, Rm::R(s.0), 0);
    }

    /// `cvtss2sd` when `to` is `D`, `cvtsd2ss` when it is `S`.
    pub fn float_to_float(&mut self, to: Prec, d: Xmm, s: Xmm) {
        let pfx = if to == Prec::D { 0xf3 } else { 0xf2 };
        self.inst(pfx, false, 0, &[0x0f, 0x5a], d.0, Rm::R(s.0), 0);
    }

    /// `roundss` or `roundsd` with the rounding `mode` of the immediate, SSE4.1.
    pub fn round(&mut self, p: Prec, d: Xmm, s: Xmm, mode: u8) {
        let op = if p == Prec::D { 0x0b } else { 0x0a };
        self.inst(0x66, false, 0, &[0x0f, 0x3a, op], d.0, Rm::R(s.0), 1);
        self.byte(mode);
    }

    /// `xorps` or `xorpd`.
    pub fn fxor(&mut self, p: Prec, d: Xmm, s: Xmm) {
        let pfx = if p == Prec::D { 0x66 } else { 0 };
        self.inst(pfx, false, 0, &[0x0f, 0x57], d.0, Rm::R(s.0), 0);
    }

    /// `andps` or `andpd`.
    pub fn fand(&mut self, p: Prec, d: Xmm, s: Xmm) {
        let pfx = if p == Prec::D { 0x66 } else { 0 };
        self.inst(pfx, false, 0, &[0x0f, 0x54], d.0, Rm::R(s.0), 0);
    }
}

#[cfg(test)]
mod tests;
