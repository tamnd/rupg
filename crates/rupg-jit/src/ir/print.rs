// Lifted from rudb crates/rudb-qc-ir/src/print.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! The text form of section 6.8 of `spec/compiler/06-qir.md`.
//!
//! One instruction per line. Values print as `%name`, where the name is the builder's hint made
//! unique, or `%vN` for a value with none. Constants print inline and take their type from the
//! operand position, so the parser can read them back without a type on every literal. The
//! printer and [`crate::ir::parse`] agree on every detail, and the round-trip test holds them to it.

use std::fmt::Write;

use crate::ir::func::{A16, DEAD, INV, NT};
use crate::ir::{Block, CATALOGUE, Class, Form, Func, Inst, Module, Ty, Val};

/// Prints a module.
#[must_use]
pub fn print(m: &Module) -> String {
    print_with(m, false)
}

/// Prints a module, with each instruction's plan node and generator line as a trailing comment
/// when `provenance` is set.
#[must_use]
pub fn print_with(m: &Module, provenance: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "module {}", m.name);
    for (i, e) in m.errors.iter().enumerate() {
        let _ = writeln!(out, "  error !E{i} {} {:?}", e.kind.name(), e.text);
    }
    for (i, g) in m.guards.iter().enumerate() {
        let inv = if g.invariant { " inv" } else { "" };
        let _ = writeln!(out, "  guard !G{i} {:?} fallback={}{inv}", g.fact, g.fallback);
    }
    for (i, c) in m.counters.iter().enumerate() {
        let _ = writeln!(out, "  counter #{i} {:?}", c.name);
    }
    for (i, k) in m.kernels.iter().enumerate() {
        let _ = writeln!(out, "  kernel @K{i} {:?}", k.name);
    }
    for (i, b) in m.blobs.iter().enumerate() {
        let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
        let _ = writeln!(out, "  blob {i} {hex}");
    }
    for f in &m.funcs {
        out.push('\n');
        func(&mut out, f, provenance);
    }
    out
}

/// Prints one function.
#[must_use]
pub fn print_func(f: &Func) -> String {
    let mut out = String::new();
    func(&mut out, f, false);
    out
}

/// The printed names of a function's values, unique within it.
#[must_use]
pub fn names(f: &Func) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(f.vals.len());
    for (i, v) in f.vals.iter().enumerate() {
        let base = match &v.name {
            Some(n) if !n.is_empty() => n
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' { c } else { '_' })
                .collect(),
            _ => format!("v{i}"),
        };
        let mut name = base.clone();
        let mut k = i;
        while !seen.insert(name.clone()) {
            name = format!("{base}.{k}");
            k += 1;
        }
        out.push(name);
    }
    out
}

struct Printer<'a> {
    f: &'a Func,
    names: Vec<String>,
}

impl Printer<'_> {
    fn val(&self, v: Val) -> String {
        match self.f.constant(v) {
            Some(c) => konst(c.ty, c.bits),
            None if v == Val::NONE => "none".to_owned(),
            None => format!("%{}", self.names.get(v.index()).map_or("?", String::as_str)),
        }
    }

    fn vals(&self, words: &[u32]) -> String {
        words.iter().map(|w| self.val(Val(*w))).collect::<Vec<_>>().join(", ")
    }

    fn target(&self, b: u32, args: &[u32]) -> String {
        if args.is_empty() { format!("b{b}") } else { format!("b{b}({})", self.vals(args)) }
    }

    fn addr(&self, base: u32, idx: u32, scale: u32, disp: u32) -> String {
        let mut s = format!("[{}", self.val(Val(base)));
        if idx != Val::NONE.0 {
            let _ = write!(s, " + {}*{scale}", self.val(Val(idx)));
        }
        let disp = disp as i32;
        if disp > 0 {
            let _ = write!(s, " + {disp}");
        } else if disp < 0 {
            let _ = write!(s, " - {}", -i64::from(disp));
        }
        s.push(']');
        s
    }

    fn inst(&self, i: &Inst<'_>) -> String {
        let o = i.ops;
        let op = i.op.name();
        let ty = i.ty.name();
        let body = match i.op.form() {
            Form::Un | Form::Bin | Form::Cmp | Form::Wide | Form::Sel => {
                format!("{op} {ty} {}", self.vals(o))
            }
            Form::StrMk => format!("{op} {}", self.vals(o)),
            Form::TrapBin => format!("{op} {ty} {}, !E{}", self.vals(&o[..2]), o[2]),
            Form::TrapUn => format!("{op} {ty} {}, !E{}", self.val(Val(o[0])), o[1]),
            Form::EdgeBin => format!("{op} {ty} {}, b{}, b{}", self.vals(&o[..2]), o[2], o[3]),
            Form::Scale => format!("{op} {ty} {}, {}", self.val(Val(o[0])), o[1]),
            Form::TrapScale => format!("{op} {ty} {}, {}, !E{}", self.val(Val(o[0])), o[1], o[2]),
            Form::Conv => {
                format!("{op} {} {} -> {ty}", self.f.ty(Val(o[0])).name(), self.val(Val(o[0])))
            }
            Form::TrapConv => {
                format!(
                    "{op} {} {} -> {ty}, !E{}",
                    self.f.ty(Val(o[0])).name(),
                    self.val(Val(o[0])),
                    o[1]
                )
            }
            Form::Load => {
                let mnemonic = if i.ty == Ty::Str16 { op.to_owned() } else { format!("{op}.{ty}") };
                format!("{mnemonic} {}", self.addr(o[0], o[1], o[2], o[3]))
            }
            Form::Store => {
                let mnemonic = if i.ty == Ty::Str16 { op.to_owned() } else { format!("{op}.{ty}") };
                format!("{mnemonic} {}, {}", self.addr(o[0], o[1], o[2], o[3]), self.val(Val(o[4])))
            }
            Form::LoadBit => format!("{op} [{} + {}]", self.val(Val(o[0])), self.val(Val(o[1]))),
            Form::Memcpy | Form::Memeq => format!("{op} {}, {}", self.vals(&o[..2]), o[2]),
            Form::Prefetch => format!("{op} {}", self.addr(o[0], o[1], o[2], o[3])),
            Form::Cas | Form::Atomic => format!("{op} {ty} {}", self.vals(o)),
            Form::Br => format!("{op} {}", self.target(o[0], &o[1..])),
            Form::Brif => {
                let n = o[2] as usize;
                format!(
                    "{op} {}, {}, {}",
                    self.val(Val(o[0])),
                    self.target(o[1], &o[3..3 + n]),
                    self.target(o[3 + n], &o[4 + n..])
                )
            }
            Form::Switch => {
                let n = o[2] as usize;
                let cases: Vec<String> = o[3 + n..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| format!("{}: b{}", c[0], c[1]))
                    .collect();
                format!(
                    "{op} {ty} {}, {}, [{}]",
                    self.val(Val(o[0])),
                    self.target(o[1], &o[3..3 + n]),
                    cases.join(", ")
                )
            }
            Form::Ret => format!("{op} {}", self.val(Val(o[0]))),
            Form::Trap => format!("{op} !E{}", o[0]),
            Form::Rtcall => {
                let name = CATALOGUE.get(o[0] as usize).map_or("?", |p| p.name);
                format!("{op} @{name}({})", self.vals(&o[1..]))
            }
            Form::Vcall => format!("{op} @K{}({})", o[0], self.vals(&o[1..])),
            Form::Guard => format!("{op} {}, !G{}", self.val(Val(o[0])), o[1]),
            Form::Poll => format!("{op} {}", o[0]),
            Form::CtrAdd => format!("{op} #{}, {}", o[0], self.val(Val(o[1]))),
        };
        let mut line = match i.result {
            Some(r) => format!("{} = {body}", self.val(r)),
            None => body,
        };
        for (bit, word) in [(NT, "nt"), (A16, "a16"), (INV, "inv"), (DEAD, "dead")] {
            if i.flags & bit != 0 {
                line.push(' ');
                line.push_str(word);
            }
        }
        if let Some(r) = i.result {
            self.class(&mut line, r);
        }
        line
    }

    fn class(&self, line: &mut String, v: Val) {
        let info = &self.f.vals[v.index()];
        if info.ty == Ty::Str16 && info.class != Class::Persistent {
            let _ = write!(line, " class={}", info.class.name());
        }
    }
}

fn func(out: &mut String, f: &Func, provenance: bool) {
    let p = Printer { f, names: names(f) };
    let sink = if f.morsel_local { " morsel-local" } else { "" };
    let _ = writeln!(out, "func @{} version={} plan=#{}{sink}", f.name, f.version, f.plan);
    for field in &f.state {
        let _ = writeln!(out, "  state {} {} {:?}", field.offset, field.size, field.name);
    }
    for (v, valid) in &f.validity {
        let _ = writeln!(out, "  valid {}, {}", p.val(*v), p.val(*valid));
    }
    for b in f.layout() {
        let data = &f.blocks[b.index()];
        let mut head = String::new();
        if data.cold {
            head.push_str("cold ");
        }
        if data.is_loop {
            let _ = write!(head, "loop({}) ", data.depth);
        }
        if data.bounded {
            head.push_str("bounded ");
        }
        if data.batch {
            head.push_str("batch ");
        }
        if head.is_empty() {
            head.push_str("block ");
        }
        let params: Vec<String> = data
            .params
            .iter()
            .map(|v| {
                let mut s = format!("{} {}", f.ty(*v).name(), p.val(*v));
                p.class(&mut s, *v);
                s
            })
            .collect();
        let _ = if params.is_empty() {
            writeln!(out, "{head}b{}:", b.0)
        } else {
            writeln!(out, "{head}b{}({}):", b.0, params.join(", "))
        };
        for (n, i) in f.insts(Block(b.0)).enumerate() {
            let line = p.inst(&i);
            if provenance {
                let site = f.sites[data.prov[n] as usize];
                let _ = writeln!(
                    out,
                    "  {line:<48} ; plan=#{} at {}:{}",
                    site.plan, site.file, site.line
                );
            } else {
                let _ = writeln!(out, "  {line}");
            }
        }
    }
}

/// A constant as the text form writes it.
#[must_use]
pub fn konst(ty: Ty, bits: u128) -> String {
    match ty {
        Ty::I1 => (if bits != 0 { "true" } else { "false" }).to_owned(),
        Ty::F64 => {
            let x = f64::from_bits(bits as u64);
            let s = format!("{x:?}");
            if x.is_finite() && s.parse::<f64>().map(f64::to_bits) == Ok(bits as u64) {
                s
            } else {
                format!("0x{bits:x}")
            }
        }
        Ty::F32 => {
            let x = f32::from_bits(bits as u32);
            let s = format!("{x:?}");
            if x.is_finite() && s.parse::<f32>().map(f32::to_bits) == Ok(bits as u32) {
                s
            } else {
                format!("0x{bits:x}")
            }
        }
        _ => {
            let s = crate::ir::eval::sext(ty, bits);
            if s < 0 && s > -(1i128 << 32) { s.to_string() } else { bits.to_string() }
        }
    }
}
