// Lifted from rudb crates/rudb-qc-direct/src/asm/tests.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the tier are under `crate::direct` and QIR is `crate::ir`.

use iced_x86::{Decoder, DecoderOptions, Formatter, IntelFormatter, MemorySizeOptions};

use super::*;

const B: [&str; 16] = [
    "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b",
    "r13b", "r14b", "r15b",
];
const W: [&str; 16] = [
    "ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w",
    "r14w", "r15w",
];
const D: [&str; 16] = [
    "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d",
    "r13d", "r14d", "r15d",
];
const Q: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];
const SIZES: [Size; 4] = [Size::B, Size::W, Size::D, Size::Q];
const CC: [&str; 16] =
    ["o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g"];

fn regs() -> impl Iterator<Item = Reg> {
    (0..16).map(Reg)
}

fn xmms() -> impl Iterator<Item = Xmm> {
    (0..16).map(Xmm)
}

fn name(size: Size, r: Reg) -> &'static str {
    let names = match size {
        Size::B => &B,
        Size::W => &W,
        Size::D => &D,
        Size::Q => &Q,
    };
    names[r.0 as usize]
}

fn ptr(size: Size) -> &'static str {
    match size {
        Size::B => "byte",
        Size::W => "word",
        Size::D => "dword",
        Size::Q => "qword",
    }
}

fn mask(size: Size, x: i64) -> u64 {
    match size {
        Size::B => u64::from(x as u8),
        Size::W => u64::from(x as u16),
        Size::D => u64::from(x as u32),
        Size::Q => x as u64,
    }
}

/// The memory operands whose encodings differ: every base, with no displacement, a short one and
/// a long one, and with an index at each scale.
fn mems() -> Vec<(Mem, String)> {
    let mut out = Vec::new();
    for base in regs() {
        for disp in [0, 8, -8, 0x1000, -0x1000] {
            out.push((Mem::at(base, disp), address(base, None, disp)));
        }
        for (index, scale) in [(Reg::RCX, 1), (Reg::R13, 2), (Reg::RBP, 4), (Reg::R12, 8)] {
            for disp in [0, 0x7f, -0x80, 0x80] {
                let m = Mem::indexed(base, index, scale, disp);
                out.push((m, address(base, Some((index, scale)), disp)));
            }
        }
    }
    out
}

fn address(base: Reg, index: Option<(Reg, u8)>, disp: i32) -> String {
    let mut s = format!("[{}", Q[base.0 as usize]);
    if let Some((i, scale)) = index {
        s += &format!("+{}", Q[i.0 as usize]);
        if scale > 1 {
            s += &format!("*{scale}");
        }
    }
    if disp > 0 {
        s += &format!("+{disp:#x}");
    } else if disp < 0 {
        s += &format!("-{:#x}", -i64::from(disp));
    }
    s + "]"
}

/// Decodes all of `code` at address 0, one instruction per line.
fn decode(code: &[u8]) -> Vec<String> {
    let mut f = IntelFormatter::new();
    let o = f.options_mut();
    o.set_hex_prefix("0x");
    o.set_hex_suffix("");
    o.set_uppercase_hex(false);
    o.set_small_hex_numbers_in_decimal(false);
    o.set_space_after_operand_separator(true);
    o.set_memory_size_options(MemorySizeOptions::Always);
    o.set_show_branch_size(false);
    o.set_branch_leading_zeros(false);
    o.set_leading_zeros(false);
    let mut d = Decoder::with_ip(64, code, 0, DecoderOptions::NONE);
    let mut out = Vec::new();
    while d.can_decode() {
        let i = d.decode();
        assert!(!i.is_invalid(), "cannot decode {code:02x?}");
        let mut s = String::new();
        f.format(&i, &mut s);
        out.push(s);
    }
    out
}

/// Encodes one instruction and checks that it decodes to `want` and to nothing else.
#[track_caller]
fn check(want: &str, f: impl FnOnce(&mut Asm)) {
    let mut a = Asm::new();
    f(&mut a);
    let got = decode(&a.code);
    assert_eq!(got, [want], "{:02x?}", a.code);
}

#[test]
fn register_moves() {
    for size in SIZES {
        for d in regs() {
            for s in regs() {
                let want = format!("mov {}, {}", name(size, d), name(size, s));
                check(&want, |a| a.mov(size, d, s));
            }
        }
    }
    for d in regs() {
        for s in regs() {
            check(&format!("xchg {}, {}", Q[s.0 as usize], Q[d.0 as usize]), |a| a.xchg(d, s));
            let (dd, sb, sw, sd) =
                (D[d.0 as usize], B[s.0 as usize], W[s.0 as usize], D[s.0 as usize]);
            check(&format!("movzx {dd}, {sb}"), |a| a.movzx(Size::B, d, s));
            check(&format!("movzx {dd}, {sw}"), |a| a.movzx(Size::W, d, s));
            check(&format!("mov {dd}, {sd}"), |a| a.movzx(Size::D, d, s));
            check(&format!("movsx {dd}, {sb}"), |a| a.movsx(Size::B, Size::D, d, s));
            let dq = Q[d.0 as usize];
            check(&format!("movsx {dq}, {sw}"), |a| a.movsx(Size::W, Size::Q, d, s));
            check(&format!("movsxd {dq}, {sd}"), |a| a.movsx(Size::D, Size::Q, d, s));
        }
    }
}

#[test]
fn immediates_take_the_shortest_form() {
    for d in regs() {
        let (dd, dq) = (D[d.0 as usize], Q[d.0 as usize]);
        for x in [0, 1, 0x7fff_ffff, 0xffff_ffff] {
            let mut a = Asm::new();
            a.mov_imm(d, x);
            assert_eq!(a.len(), 5 + usize::from(d.0 >= 8));
            check(&format!("mov {dd}, {x:#x}"), |a| a.mov_imm(d, x));
        }
        for x in [-1i64, -0x8000_0000] {
            check(&format!("mov {dq}, {:#x}", x as u64), |a| a.mov_imm(d, x as u64));
        }
        for x in [0x1_0000_0000u64, 0x8000_0000_0000_0000, 0x1234_5678_9abc_def0] {
            let mut a = Asm::new();
            a.mov_imm(d, x);
            assert_eq!(a.len(), 10);
            check(&format!("mov {dq}, {x:#x}"), |a| a.mov_imm(d, x));
        }
    }
}

#[test]
fn loads_and_stores_at_every_address() {
    for (m, at) in mems() {
        for r in [Reg::RAX, Reg::RSI, Reg::R9, Reg::R15] {
            let i = r.0 as usize;
            check(&format!("movzx {}, byte ptr {at}", D[i]), |a| a.load(Size::B, r, m));
            check(&format!("movzx {}, word ptr {at}", D[i]), |a| a.load(Size::W, r, m));
            check(&format!("mov {}, dword ptr {at}", D[i]), |a| a.load(Size::D, r, m));
            check(&format!("mov {}, qword ptr {at}", Q[i]), |a| a.load(Size::Q, r, m));
            check(&format!("movsx {}, byte ptr {at}", Q[i]), |a| a.load_sx(Size::B, Size::Q, r, m));
            check(&format!("movsx {}, word ptr {at}", D[i]), |a| a.load_sx(Size::W, Size::D, r, m));
            check(&format!("movsxd {}, dword ptr {at}", Q[i]), |a| {
                a.load_sx(Size::D, Size::Q, r, m);
            });
            check(&format!("lea {}, {}", Q[i], at), |a| a.lea(r, m));
            check(&format!("bt qword ptr {at}, {}", Q[i]), |a| a.bt_mem(m, r));
            for size in SIZES {
                let want = format!("mov {} ptr {at}, {}", ptr(size), name(size, r));
                check(&want, |a| a.store(size, m, r));
            }
        }
        for size in SIZES {
            for x in [0, 5, -1] {
                let want = format!("mov {} ptr {at}, {:#x}", ptr(size), mask(size, x.into()));
                check(&want, |a| a.store_imm(size, m, x));
            }
        }
    }
}

#[test]
fn the_byte_registers_past_bl_need_a_rex_prefix() {
    for s in [Reg::RSP, Reg::RBP, Reg::RSI, Reg::RDI] {
        let m = Mem::at(Reg::RAX, 0);
        check(&format!("mov byte ptr [rax], {}", B[s.0 as usize]), |a| a.store(Size::B, m, s));
        check(&format!("sete {}", B[s.0 as usize]), |a| a.setcc(Cc::E, s));
    }
}

#[test]
fn arithmetic() {
    let ops = [
        (Alu::Add, "add"),
        (Alu::Or, "or"),
        (Alu::Adc, "adc"),
        (Alu::Sbb, "sbb"),
        (Alu::And, "and"),
        (Alu::Sub, "sub"),
        (Alu::Xor, "xor"),
        (Alu::Cmp, "cmp"),
    ];
    let m = Mem::indexed(Reg::R13, Reg::R12, 8, -16);
    let at = address(Reg::R13, Some((Reg::R12, 8)), -16);
    for (op, text) in ops {
        for size in SIZES {
            for d in regs() {
                for s in [Reg::RAX, Reg::RSP, Reg::RDI, Reg::R8, Reg::R15] {
                    let (dn, sn) = (name(size, d), name(size, s));
                    check(&format!("{text} {dn}, {sn}"), |a| a.alu(op, size, d, s));
                }
                for x in [1, -1, 0x7f, -0x80, 0x1234, 0x7fff_ffff] {
                    let x = if size == Size::B { x & 0xff } else { x };
                    let (dn, xs) = (name(size, d), mask(size, x.into()));
                    check(&format!("{text} {dn}, {xs:#x}"), |a| a.alu_imm(op, size, d, x));
                }
                let dn = name(size, d);
                check(&format!("{text} {dn}, {} ptr {at}", ptr(size)), |a| {
                    a.alu_load(op, size, d, m);
                });
                check(&format!("{text} {} ptr {at}, {dn}", ptr(size)), |a| {
                    a.alu_store(op, size, m, d);
                });
            }
            for x in [3, -2, 0x100] {
                let x = if size == Size::B { x & 0xff } else { x };
                let want = format!("{text} {} ptr {at}, {:#x}", ptr(size), mask(size, x.into()));
                check(&want, |a| a.alu_mem_imm(op, size, m, x));
            }
        }
    }
}

#[test]
fn multiply_divide_and_the_rest_of_the_integer_forms() {
    let unary = [
        (Unary::Not, "not"),
        (Unary::Neg, "neg"),
        (Unary::Mul, "mul"),
        (Unary::Imul, "imul"),
        (Unary::Div, "div"),
        (Unary::Idiv, "idiv"),
    ];
    let shifts = [
        (Shift::Rol, "rol"),
        (Shift::Ror, "ror"),
        (Shift::Shl, "shl"),
        (Shift::Shr, "shr"),
        (Shift::Sar, "sar"),
    ];
    for size in SIZES {
        for d in regs() {
            let dn = name(size, d);
            for (op, text) in unary {
                check(&format!("{text} {dn}"), |a| a.unary(op, size, d));
            }
            for (op, text) in shifts {
                check(&format!("{text} {dn}, cl"), |a| a.shift_cl(op, size, d));
                check(&format!("{text} {dn}, 0x1"), |a| a.shift_imm(op, size, d, 1));
                check(&format!("{text} {dn}, 0x7"), |a| a.shift_imm(op, size, d, 7));
            }
            for s in regs() {
                let sn = name(size, s);
                check(&format!("test {dn}, {sn}"), |a| a.test(size, d, s));
                if size != Size::B {
                    check(&format!("imul {dn}, {sn}"), |a| a.imul(size, d, s));
                    for x in [3, -3, 0x1000] {
                        // The disassembler writes the two operand form when they are the same.
                        let xs = mask(size, x.into());
                        let want = if d == s {
                            format!("imul {dn}, {xs:#x}")
                        } else {
                            format!("imul {dn}, {sn}, {xs:#x}")
                        };
                        check(&want, |a| a.imul_imm(size, d, s, x));
                    }
                }
            }
            for x in [1, 0x80] {
                let xs = mask(size, x);
                check(&format!("test {dn}, {xs:#x}"), |a| a.test_imm(size, d, x as i32));
            }
            if size != Size::B {
                check(&format!("bt {dn}, 0x3"), |a| a.bt_imm(size, d, 3));
            }
        }
    }
    check("cwd", |a| a.sign_extend_ax(Size::W));
    check("cdq", |a| a.sign_extend_ax(Size::D));
    check("cqo", |a| a.sign_extend_ax(Size::Q));
}

#[test]
fn flags_bits_and_bytes() {
    let bits = [
        (Bits::Bsf, "bsf"),
        (Bits::Bsr, "bsr"),
        (Bits::Tzcnt, "tzcnt"),
        (Bits::Lzcnt, "lzcnt"),
        (Bits::Popcnt, "popcnt"),
    ];
    for d in regs() {
        for (i, cc) in Cc::ALL.into_iter().enumerate() {
            assert_eq!(cc as usize, i);
            assert_eq!(cc.invert().invert(), cc);
            check(&format!("set{} {}", CC[i], B[d.0 as usize]), |a| a.setcc(cc, d));
        }
        for s in regs() {
            for size in [Size::D, Size::Q] {
                let (dn, sn) = (name(size, d), name(size, s));
                check(&format!("cmove {dn}, {sn}"), |a| a.cmov(Cc::E, size, d, s));
                check(&format!("cmovl {dn}, {sn}"), |a| a.cmov(Cc::L, size, d, s));
            }
            for size in [Size::W, Size::D, Size::Q] {
                for (op, text) in bits {
                    let (dn, sn) = (name(size, d), name(size, s));
                    check(&format!("{text} {dn}, {sn}"), |a| a.bits(op, size, d, s));
                }
            }
            check(&format!("crc32 {}, {}", Q[d.0 as usize], Q[s.0 as usize]), |a| a.crc32(d, s));
            for size in [Size::W, Size::D, Size::Q] {
                let (dn, sn) = (name(size, d), name(size, s));
                check(&format!("bt {dn}, {sn}"), |a| a.bt(size, d, s));
            }
        }
        check(&format!("bswap {}", D[d.0 as usize]), |a| a.bswap(Size::D, d));
        check(&format!("bswap {}", Q[d.0 as usize]), |a| a.bswap(Size::Q, d));
        check(&format!("push {}", Q[d.0 as usize]), |a| a.push(d));
        check(&format!("pop {}", Q[d.0 as usize]), |a| a.pop(d));
    }
    check("ret", Asm::ret);
    check("ud2", Asm::ud2);
}

#[test]
fn branches_patch_forward_and_go_short_backward() {
    let mut a = Asm::new();
    let (top, out, table) = (a.label(), a.label(), a.label());
    a.bind(top);
    a.alu_imm(Alu::Sub, Size::Q, Reg::RCX, 1);
    a.jcc(Cc::Ne, top);
    a.jcc(Cc::E, out);
    a.call_rip(table);
    a.jmp(top);
    a.jmp(out);
    a.bind(out);
    a.ret();
    a.align(8);
    a.bind(table);
    a.code.extend_from_slice(&[0; 8]);
    a.finish().unwrap();
    let want = [
        "sub rcx, 0x1",
        "jne 0x0",
        "je 0x19",
        "call qword ptr [0x20]",
        "jmp 0x0",
        "jmp 0x19",
        "ret",
        "nop",
        "nop",
        "nop",
        "nop",
        "nop",
        "nop",
        "add byte ptr [rax], al",
        "add byte ptr [rax], al",
        "add byte ptr [rax], al",
        "add byte ptr [rax], al",
    ];
    assert_eq!(decode(&a.code), want);
    // The backward branches are two bytes and the forward ones are rel32.
    assert_eq!(a.offset(out), Some(0x19));
    assert_eq!(&a.code[4..6], &[0x75, 0xfa]);
}

#[test]
fn a_branch_back_out_of_rel8_range_is_long() {
    let mut a = Asm::new();
    let top = a.label();
    a.bind(top);
    for _ in 0..126 {
        a.byte(0x90);
    }
    a.jmp(top);
    assert_eq!(a.len(), 128);
    a.jmp(top);
    assert_eq!(a.len(), 133);
    a.finish().unwrap();
    let got = decode(&a.code);
    assert_eq!(got[126..], ["jmp 0x0", "jmp 0x0"]);
}

#[test]
fn a_label_that_is_never_placed_is_an_error() {
    let mut a = Asm::new();
    let l = a.label();
    a.jmp(l);
    assert_eq!(a.finish(), Err(l));
}

#[test]
fn sse() {
    let fops = [
        (Fop::Sqrt, "sqrt"),
        (Fop::Add, "add"),
        (Fop::Mul, "mul"),
        (Fop::Sub, "sub"),
        (Fop::Min, "min"),
        (Fop::Div, "div"),
        (Fop::Max, "max"),
    ];
    for (p, s, dq) in [(Prec::S, "ss", "d"), (Prec::D, "sd", "q")] {
        let (ptr, pk) = if p == Prec::D { ("qword", "pd") } else { ("dword", "ps") };
        for x in xmms() {
            for y in xmms() {
                for (op, text) in fops {
                    check(&format!("{text}{s} xmm{}, xmm{}", x.0, y.0), |a| a.farith(op, p, x, y));
                }
                check(&format!("ucomi{s} xmm{}, xmm{}", x.0, y.0), |a| a.ucomis(p, x, y));
                check(&format!("xor{pk} xmm{}, xmm{}", x.0, y.0), |a| a.fxor(p, x, y));
                check(&format!("and{pk} xmm{}, xmm{}", x.0, y.0), |a| a.fand(p, x, y));
                check(&format!("round{s} xmm{}, xmm{}, 0x9", x.0, y.0), |a| a.round(p, x, y, 9));
                let to = if p == Prec::D { "cvtss2sd" } else { "cvtsd2ss" };
                check(&format!("{to} xmm{}, xmm{}", x.0, y.0), |a| a.float_to_float(p, x, y));
                if p == Prec::S {
                    check(&format!("movaps xmm{}, xmm{}", x.0, y.0), |a| a.fmov(x, y));
                }
            }
            for r in regs() {
                let (rn, gn) = (Q[r.0 as usize], if p == Prec::D { Q } else { D }[r.0 as usize]);
                check(&format!("mov{dq} xmm{}, {gn}", x.0), |a| a.to_xmm(p, x, r));
                check(&format!("mov{dq} {gn}, xmm{}", x.0), |a| a.from_xmm(p, r, x));
                check(&format!("cvtsi2{s} xmm{}, {rn}", x.0), |a| a.int_to_float(p, x, r));
                check(&format!("cvtt{s}2si {rn}, xmm{}", x.0), |a| a.float_to_int(p, r, x));
            }
        }
        for (m, at) in mems() {
            for x in [Xmm(0), Xmm(7), Xmm(8), Xmm(15)] {
                check(&format!("mov{s} xmm{}, {ptr} ptr {at}", x.0), |a| a.fload(p, x, m));
                check(&format!("mov{s} {ptr} ptr {at}, xmm{}", x.0), |a| a.fstore(p, m, x));
                if p == Prec::S {
                    check(&format!("movups xmm{}, xmmword ptr {at}", x.0), |a| a.vload(x, m));
                    check(&format!("movups xmmword ptr {at}, xmm{}", x.0), |a| a.vstore(m, x));
                }
            }
        }
    }
}
