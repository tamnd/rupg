//! The `jit_verify` target of spec/21 section 21.7: a pipeline in QIR text, run on the interpreter and on each tier of machine code that the build has. Compiled code that gives another status or other bytes of the state than the interpreter is a failure.
//!
//! The input is not the text itself. Text that libFuzzer changes byte by byte seldom parses, and a pipeline that loads from a pointer of its own making reads memory that is not there, which is a fault of the input and not of a tier. So [`program`] reads the bytes as a program: the types and the values of the inputs, and then a list of instructions, each an opcode, the operands from the values before it, and a type or a constant where the opcode takes one. Each instruction is kept only when the module with it parses and verifies. The function loads the inputs from fixed slots of the state and stores each value it computes into a slot of its own, so every value is used, as the verifier wants, and every value is compared.

use std::fmt::Write as _;
use std::sync::Arc;

use rupg_jit::ir::{Form, Module, Op, Ty, parse, verify};
use rupg_jit::rt::Rt;
use rupg_jit::tier::{Options, Platform, Tier, Tiers};
use rupg_platform::sim::SimClock;

/// The types of the values of a program. A pointer and a `str16` are left out, as their loads would read memory that the state does not hold.
const TYPES: [Ty; 8] = [Ty::I1, Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128, Ty::F32, Ty::F64];

/// The forms of the instructions of a program. They compute on values with no memory and no control flow.
const FORMS: [Form; 11] = [
    Form::Un,
    Form::Bin,
    Form::Cmp,
    Form::Sel,
    Form::Wide,
    Form::TrapBin,
    Form::TrapUn,
    Form::Conv,
    Form::TrapConv,
    Form::Scale,
    Form::TrapScale,
];

/// The inputs of a program, in the slots after the header of the state.
const INPUTS: usize = 4;

/// The instructions of a program at most.
const STEPS: usize = 32;

/// The first slot of the state that holds an input. The slots before it are the header, which the native tiers write.
const FIRST: usize = 4;

/// The slots of the state: the header, the inputs and a slot for each value.
pub const SLOTS: usize = FIRST + INPUTS + INPUTS + STEPS;

/// The bytes of the input of the target, read from the front.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    /// 16 bytes in little-endian order, with zeros for the bytes past the end.
    fn wide(&mut self) -> u128 {
        let mut w = [0u8; 16];
        let n = self.0.len().min(16);
        w[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        u128::from_le_bytes(w)
    }
}

/// A program of the target: the text of its module and the state that it starts on.
#[derive(Debug)]
pub struct Program {
    /// The QIR text of the module, of one function.
    pub text: String,
    /// The state, with the inputs in their slots.
    pub state: [u128; SLOTS],
}

/// The text of the module whose function loads the inputs `inputs`, runs `lines` and stores each value `values` into its slot.
fn module(inputs: &[Ty], lines: &[String], values: &[Ty]) -> String {
    let mut text = String::from(
        "module fuzz\n  error !E0 overflow \"x\"\n\nfunc @fuzz version=generic plan=#1\nblock b0(ptr %st, ptr %m):\n",
    );
    for (i, ty) in inputs.iter().enumerate() {
        let _ = writeln!(text, "  %v{i} = load.{} [%st + {}]", ty.name(), (FIRST + i) * 16);
    }
    for line in lines {
        let _ = writeln!(text, "  {line}");
    }
    for (i, ty) in values.iter().enumerate() {
        let _ =
            writeln!(text, "  store.{} [%st + {}], %v{i}", ty.name(), (FIRST + INPUTS + i) * 16);
    }
    text.push_str("  ret 0\n");
    text
}

/// The program of the bytes `data`, or `None` when the bytes give no module that verifies.
#[must_use]
pub fn program(data: &[u8]) -> Option<Program> {
    let mut bytes = Bytes(data);
    let mut state = [0u128; SLOTS];
    let mut inputs = Vec::with_capacity(INPUTS);
    for i in 0..INPUTS {
        inputs.push(TYPES[usize::from(bytes.byte()?) % TYPES.len()]);
        state[FIRST + i] = bytes.wide();
    }
    let mut values = inputs.clone();
    let mut lines: Vec<String> = Vec::new();
    let ops: Vec<Op> = Op::all().iter().copied().filter(|op| FORMS.contains(&op.form())).collect();
    while lines.len() < STEPS {
        let Some(b) = bytes.byte() else { break };
        let op = ops[usize::from(b) % ops.len()];
        let pick = |bytes: &mut Bytes<'_>, want: Option<Ty>| -> Option<usize> {
            let at = usize::from(bytes.byte()?);
            let fit: Vec<usize> =
                (0..values.len()).filter(|&v| want.is_none_or(|t| values[v] == t)).collect();
            (!fit.is_empty()).then(|| fit[at % fit.len()])
        };
        let Some(a) = pick(&mut bytes, None) else { break };
        let ty = values[a];
        let (t, name) = (ty.name(), op.name());
        let n = values.len();
        let (line, result) = match op.form() {
            Form::Un => (format!("%v{n} = {name} {t} %v{a}"), op.result(ty)),
            Form::Bin | Form::Cmp | Form::Wide => {
                let Some(b) = pick(&mut bytes, Some(ty)) else { break };
                (format!("%v{n} = {name} {t} %v{a}, %v{b}"), op.result(ty))
            }
            Form::Sel => {
                let Some(c) = pick(&mut bytes, Some(Ty::I1)) else { continue };
                let Some(b) = pick(&mut bytes, Some(ty)) else { break };
                (format!("%v{n} = {name} {t} %v{c}, %v{a}, %v{b}"), ty)
            }
            Form::TrapBin => {
                let Some(b) = pick(&mut bytes, Some(ty)) else { break };
                (format!("%v{n} = {name} {t} %v{a}, %v{b}, !E0"), ty)
            }
            Form::TrapUn => (format!("%v{n} = {name} {t} %v{a}, !E0"), ty),
            Form::Conv | Form::TrapConv => {
                let to = TYPES[usize::from(bytes.byte()?) % TYPES.len()];
                let trap = if op.form() == Form::TrapConv { ", !E0" } else { "" };
                (format!("%v{n} = {name} {t} %v{a} -> {}{trap}", to.name()), to)
            }
            Form::Scale | Form::TrapScale => {
                let k = bytes.byte()? % 39;
                let trap = if op.form() == Form::TrapScale { ", !E0" } else { "" };
                (format!("%v{n} = {name} {t} %v{a}, {k}{trap}"), ty)
            }
            _ => continue,
        };
        if result == Ty::Void {
            continue;
        }
        lines.push(line);
        values.push(result);
        let text = module(&inputs, &lines, &values);
        if !parse(&text).is_ok_and(|m| verify(&m).is_ok()) {
            lines.pop();
            values.pop();
        }
    }
    let text = module(&inputs, &lines, &values);
    let m = parse(&text).ok()?;
    verify(&m).ok()?;
    Some(Program { text, state })
}

/// The module of `text` on the tier `tier`, with its function compiled up front.
fn tiers(m: &Module, tier: Tier) -> Tiers {
    let platform = Platform { clock: Arc::new(SimClock::new(0)), tasks: None };
    let tiers = Tiers::new(m, Options { tier, stay: true, ..Options::default() }, &platform);
    tiers.prepare_all(m);
    tiers
}

/// The status and the state after a run of the function of `tiers` on `state`.
fn run(tiers: &Tiers, native: bool, state: &[u128; SLOTS]) -> (u64, [u128; SLOTS]) {
    let mut state = *state;
    let mut rt = Rt::new(Arc::default());
    let status = tiers.call(native, 0, state.as_mut_ptr().cast(), std::ptr::null(), &mut rt);
    (status, state)
}

/// The text of the difference between the interpreter and the tier `tier` on `program`, or `None` when they agree.
#[must_use]
pub fn differs(program: &Program, tier: Tier) -> Option<String> {
    let m = parse(&program.text).expect("the program parses");
    let want = run(&tiers(&m, Tier::Interp), false, &program.state);
    let native = tiers(&m, tier);
    // A function that the tier does not lower runs on the interpreter, but a code generator that panics has a bug.
    if let Some(why) = native.report().fallbacks.iter().find(|why| why.contains("panicked")) {
        return Some(format!("{why} on\n{}", program.text));
    }
    let got = run(&native, true, &program.state);
    (want != got).then(|| {
        format!(
            "{tier} differs from the interpreter on\n{}\nwith the inputs {:x?}:\ninterp {} {:x?}\n{tier} {} {:x?}",
            program.text,
            &program.state[FIRST..FIRST + INPUTS],
            want.0,
            &want.1[FIRST..],
            got.0,
            &got.1[FIRST..],
        )
    })
}

/// The `jit_verify` target: the program of `data` gives the status and the state of the interpreter on each tier of machine code.
///
/// # Panics
///
/// When a tier of machine code differs from the interpreter.
pub fn jit_verify(data: &[u8]) {
    let Some(program) = program(data) else { return };
    for tier in [Tier::Clif, Tier::Direct] {
        if tier.built()
            && let Some(diff) = differs(&program, tier)
        {
            panic!("{diff}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_platform::sim::SimRng;

    /// The bytes of the four inputs `inputs`, each a type and a value, and then `rest`.
    fn bytes(inputs: [(u8, u128); INPUTS], rest: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        for (ty, value) in inputs {
            data.push(ty);
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(rest);
        data
    }

    /// The inputs load from the slots after the header, each value goes to a slot of its own, and the program runs the same on every tier.
    #[test]
    fn a_program_loads_its_inputs_and_stores_every_value() {
        // Two i32 inputs of i32::MAX and 1 and two f64 inputs, with no instructions.
        let data =
            bytes([(3, i32::MAX as u128), (3, 1), (7, 1.5f64.to_bits().into()), (7, 0)], &[]);
        let p = program(&data).expect("the inputs alone verify");
        assert!(p.text.contains("%v0 = load.i32 [%st + 64]"), "{}", p.text);
        assert!(p.text.contains("%v3 = load.f64 [%st + 112]"), "{}", p.text);
        assert!(p.text.contains("store.f64 [%st + 176], %v3"), "{}", p.text);
        assert_eq!(p.state[FIRST], i32::MAX as u128);
        jit_verify(&data);
    }

    /// A select of 128 bits on a compare of 128 bits, which made the Cranelift tier panic on aarch64.
    #[test]
    fn a_select_of_128_bits_on_a_compare_of_128_bits_compiles() {
        let mut p =
            program(&bytes([(5, 7), (5, 9), (0, 0), (0, 0)], &[])).expect("the inputs verify");
        p.text = p.text.replace(
            "  store.i128 [%st + 128], %v0",
            "  %c = icmp.uge i128 %v0, %v1\n  %r = select i128 %c, %v0, %v1\n  store.i128 [%st + 400], %r\n  store.i128 [%st + 128], %v0",
        );
        for tier in [Tier::Clif, Tier::Direct].into_iter().filter(|t| t.built()) {
            assert_eq!(differs(&p, tier), None);
        }
    }

    /// Random bytes give programs that verify, that the tiers of machine code compile, and that run the same on every tier.
    #[test]
    fn random_programs_run_the_same_on_every_tier() {
        let mut rng = SimRng::new(11);
        let (mut kept, mut steps, mut native) = (0, 0, 0);
        for _ in 0..300 {
            let len = 64 + usize::try_from(rng.next_u64() % 256).unwrap_or(0);
            let data: Vec<u8> = (0..len).map(|_| rng.next_u64().to_le_bytes()[0]).collect();
            jit_verify(&data);
            let Some(p) = program(&data) else { continue };
            kept += 1;
            steps += p.text.lines().filter(|l| l.contains(" = ") && !l.contains("load.")).count();
            let m = parse(&p.text).expect("the program parses");
            if [Tier::Clif, Tier::Direct]
                .into_iter()
                .filter(|t| t.built())
                .all(|t| tiers(&m, t).report().native == 1)
            {
                native += 1;
            }
        }
        assert!(kept > 250, "only {kept} programs verified");
        assert!(steps > kept * 8, "only {steps} instructions in {kept} programs");
        // A tier that refused most programs would leave the interpreter to run against itself.
        assert!(native > kept * 9 / 10, "the tiers compiled only {native} of {kept} programs");
    }
}
