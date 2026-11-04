//! The expression language (`docs/compositing.md#expressions`): ffmpeg's function names and
//! semantics over `f64`, plus easing, `lerp`, `smoothstep`, and stateless `random` and `noise`.
//!
//! A source string is tokenised, parsed by precedence climbing into an AST, resolved against a
//! [`Scope`] (names to variable slots), constant-folded and flattened to stack ops. Evaluating
//! the ops is a pure function of the [`Frame`]: no state, no loops, so it always terminates.

#![allow(clippy::many_single_char_names)] // maths: x, y, z, a, b

use std::f64::consts::{E, PI};

use super::{Ease, Frame, Scope};

/// Longest accepted source, bytes.
pub const MAX_LEN: usize = 4096;
/// Deepest accepted nesting (parentheses, calls, unary operators, the right side of `^`, and
/// each operator of a chain like `a + b + c`, which nests to the left).
pub const MAX_DEPTH: usize = 64;
/// Most arguments a call takes (`min`, `max`).
pub const MAX_ARGS: usize = 255;

/// The golden ratio, ffmpeg's `PHI`.
const PHI: f64 = 1.618_033_988_749_895;

/// A compile error: what is wrong and where (1-based column, in characters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError {
    pub col: usize,
    pub msg: String,
}

impl ExprError {
    fn new(col: usize, msg: impl Into<String>) -> Self {
        Self {
            col,
            msg: msg.into(),
        }
    }
}

type Res<T> = std::result::Result<T, ExprError>;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Name(String),
    /// Operators and punctuation: `+ - * / % ^ ! < <= > >= == != && || ? : ( ) ,`.
    Sym(&'static str),
    End,
}

const SYMS: [&str; 21] = [
    "<=", ">=", "==", "!=", "&&", "||", "+", "-", "*", "/", "%", "^", "!", "<", ">", "?", ":", "(",
    ")", ",", "=",
];

fn lex(src: &str) -> Res<Vec<(Tok, usize)>> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let col = i + 1;
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit()
            || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i < chars.len() && chars[i] == '.' {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < chars.len() && matches!(chars[i], 'e' | 'E') {
                let mut j = i + 1;
                if j < chars.len() && matches!(chars[j], '+' | '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                } else {
                    return Err(ExprError::new(j + 1, "expected exponent digits"));
                }
            }
            let s: String = chars[start..i].iter().collect();
            let v = s
                .parse()
                .map_err(|_| ExprError::new(col, format!("bad number `{s}`")))?;
            out.push((Tok::Num(v), col));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push((Tok::Name(chars[start..i].iter().collect()), col));
        } else {
            let two: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let sym = SYMS
                .iter()
                .find(|s| two.starts_with(**s))
                .ok_or_else(|| ExprError::new(col, format!("unexpected `{c}`")))?;
            if *sym == "=" {
                return Err(ExprError::new(col, "unexpected `=` (comparison is `==`)"));
            }
            i += sym.len();
            out.push((Tok::Sym(sym), col));
        }
    }
    out.push((Tok::End, chars.len() + 1));
    Ok(out)
}

/// The parsed, unresolved tree.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Num(f64),
    Var(String, usize),
    /// An operator (`neg`, `not`, `+`, ...) or a call, with the column of its name.
    Call(String, usize, Vec<Node>),
}

impl Node {
    /// Every variable name in the tree (for ordering `vars`).
    pub(crate) fn names(&self, out: &mut Vec<String>) {
        match self {
            Self::Num(_) => {}
            Self::Var(n, _) => out.push(n.clone()),
            Self::Call(_, _, a) => {
                for a in a {
                    a.names(out);
                }
            }
        }
    }
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    i: usize,
    depth: usize,
}

/// Binary operators by binding power (higher binds tighter).
fn binary(t: &Tok) -> Option<(u8, &'static str)> {
    let Tok::Sym(s) = t else { return None };
    Some(match *s {
        "||" => (1, "||"),
        "&&" => (2, "&&"),
        "<" | "<=" | ">" | ">=" | "==" | "!=" => (3, s),
        "+" | "-" => (4, s),
        "*" | "/" | "%" => (5, s),
        _ => return None,
    })
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].0
    }

    fn col(&self) -> usize {
        self.toks[self.i].1
    }

    fn next(&mut self) -> (Tok, usize) {
        let t = self.toks[self.i].clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn expect(&mut self, s: &'static str) -> Res<()> {
        if *self.peek() == Tok::Sym(s) {
            self.next();
            Ok(())
        } else {
            Err(ExprError::new(self.col(), format!("expected \"{s}\"")))
        }
    }

    fn enter(&mut self) -> Res<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ExprError::new(
                self.col(),
                format!("nested deeper than {MAX_DEPTH} levels"),
            ));
        }
        Ok(())
    }

    fn expr(&mut self) -> Res<Node> {
        self.enter()?;
        let c = self.binary(1)?;
        let out = if *self.peek() == Tok::Sym("?") {
            let col = self.col();
            self.next();
            let a = self.expr()?;
            self.expect(":")?;
            let b = self.expr()?;
            Node::Call("?:".into(), col, vec![c, a, b])
        } else {
            c
        };
        self.depth -= 1;
        Ok(out)
    }

    fn binary(&mut self, min: u8) -> Res<Node> {
        let mut lhs = self.unary()?;
        // Each operator of a chain nests the tree one level deeper on the left.
        let mut chain = 0;
        let out = loop {
            let Some((p, op)) = binary(self.peek()) else {
                break Ok(lhs);
            };
            if p < min {
                break Ok(lhs);
            }
            if let Err(e) = self.enter() {
                break Err(e);
            }
            chain += 1;
            let col = self.col();
            self.next();
            let rhs = match self.binary(p + 1) {
                Ok(r) => r,
                Err(e) => break Err(e),
            };
            if p == 3 && binary(self.peek()).is_some_and(|(q, _)| q == 3) {
                break Err(ExprError::new(
                    self.col(),
                    "comparisons don't chain: use `&&`",
                ));
            }
            lhs = Node::Call(op.into(), col, vec![lhs, rhs]);
        };
        self.depth -= chain;
        out
    }

    fn unary(&mut self) -> Res<Node> {
        let col = self.col();
        let op = match self.peek() {
            Tok::Sym("-") => "neg",
            Tok::Sym("+") => "pos",
            Tok::Sym("!") => "!",
            _ => return self.pow(),
        };
        self.next();
        self.enter()?;
        let a = self.unary()?;
        self.depth -= 1;
        Ok(if op == "pos" {
            a
        } else {
            Node::Call(op.into(), col, vec![a])
        })
    }

    fn pow(&mut self) -> Res<Node> {
        let base = self.atom()?;
        if *self.peek() != Tok::Sym("^") {
            return Ok(base);
        }
        let col = self.col();
        self.next();
        self.enter()?;
        let e = self.unary()?;
        self.depth -= 1;
        Ok(Node::Call("^".into(), col, vec![base, e]))
    }

    fn atom(&mut self) -> Res<Node> {
        let (t, col) = self.next();
        match t {
            Tok::Num(v) => Ok(Node::Num(v)),
            Tok::Name(n) => {
                if *self.peek() != Tok::Sym("(") {
                    return Ok(Node::Var(n, col));
                }
                self.next();
                self.enter()?;
                let mut args = Vec::new();
                if *self.peek() != Tok::Sym(")") {
                    args.push(self.expr()?);
                    while *self.peek() == Tok::Sym(",") {
                        if args.len() == MAX_ARGS {
                            return Err(ExprError::new(
                                col,
                                format!("{n}() takes at most {MAX_ARGS} arguments"),
                            ));
                        }
                        self.next();
                        args.push(self.expr()?);
                    }
                }
                self.expect(")")?;
                self.depth -= 1;
                Ok(Node::Call(n, col, args))
            }
            Tok::Sym("(") => {
                self.enter()?;
                let e = self.expr()?;
                self.expect(")")?;
                self.depth -= 1;
                Ok(e)
            }
            Tok::End => Err(ExprError::new(col, "unexpected end")),
            Tok::Sym(s) => Err(ExprError::new(col, format!("unexpected \"{s}\""))),
        }
    }
}

/// Parses `src` into its tree (no name resolution).
pub(crate) fn parse(src: &str) -> Res<Node> {
    if src.len() > MAX_LEN {
        return Err(ExprError::new(1, format!("longer than {MAX_LEN} bytes")));
    }
    let mut p = Parser {
        toks: lex(src)?,
        i: 0,
        depth: 0,
    };
    let e = p.expr()?;
    match p.peek() {
        Tok::End => Ok(e),
        _ => Err(ExprError::new(p.col(), "expected an operator or the end")),
    }
}

/// An operation on the stack: its arguments are the top `arity` values.
#[derive(Debug, Clone, Copy, PartialEq)]
enum F {
    Neg,
    Not,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    /// `c, a, b`: `a` if `c` is non-zero, else `b`.
    Select,
    Abs,
    Floor,
    Ceil,
    Round,
    Trunc,
    Sqrt,
    Exp,
    Log,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Hypot,
    Sign,
    Squish,
    Gauss,
    Between,
    Clip,
    Min,
    Max,
    Lerp,
    Smoothstep,
    Ease(Ease),
    Bezier,
    /// `seed, n`.
    Random,
    Noise,
}

fn b(c: bool) -> f64 {
    if c { 1.0 } else { 0.0 }
}

impl F {
    fn apply(self, a: &[f64]) -> f64 {
        let x = a.first().copied().unwrap_or(0.0);
        let y = a.get(1).copied().unwrap_or(0.0);
        let z = a.get(2).copied().unwrap_or(0.0);
        match self {
            Self::Neg => -x,
            Self::Not => b(x == 0.0),
            Self::Add => x + y,
            Self::Sub => x - y,
            Self::Mul => x * y,
            Self::Div => x / y,
            // ffmpeg's `mod`: `x - floor(x / y) * y`.
            Self::Mod => x - (x / y).floor() * y,
            Self::Pow => x.powf(y),
            Self::Lt => b(x < y),
            Self::Le => b(x <= y),
            Self::Gt => b(x > y),
            Self::Ge => b(x >= y),
            #[allow(clippy::float_cmp)]
            Self::Eq => b(x == y),
            #[allow(clippy::float_cmp)]
            Self::Ne => b(x != y),
            Self::And => b(x != 0.0 && y != 0.0),
            Self::Or => b(x != 0.0 || y != 0.0),
            Self::Select => {
                if x == 0.0 {
                    z
                } else {
                    y
                }
            }
            Self::Abs => x.abs(),
            Self::Floor => x.floor(),
            Self::Ceil => x.ceil(),
            Self::Round => x.round(),
            Self::Trunc => x.trunc(),
            Self::Sqrt => x.sqrt(),
            Self::Exp => x.exp(),
            Self::Log => x.ln(),
            Self::Sin => x.sin(),
            Self::Cos => x.cos(),
            Self::Tan => x.tan(),
            Self::Asin => x.asin(),
            Self::Acos => x.acos(),
            Self::Atan => x.atan(),
            Self::Atan2 => x.atan2(y),
            Self::Hypot => x.hypot(y),
            Self::Sign => {
                if x > 0.0 {
                    1.0
                } else if x < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            }
            Self::Squish => 1.0 / (1.0 + (4.0 * x).exp()),
            Self::Gauss => (-x * x / 2.0).exp() / (2.0 * PI).sqrt(),
            Self::Between => b(x >= y && x <= z),
            Self::Clip => {
                if x.is_nan() || y.is_nan() || z.is_nan() || y > z {
                    f64::NAN
                } else {
                    x.clamp(y, z)
                }
            }
            Self::Min => a.iter().copied().fold(f64::INFINITY, f64::min),
            Self::Max => a.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            Self::Lerp => x + (y - x) * z,
            Self::Smoothstep => {
                let u = ((z - x) / (y - x)).clamp(0.0, 1.0);
                u * u * (3.0 - 2.0 * u)
            }
            Self::Ease(e) => e.at(x),
            Self::Bezier => Ease::Bezier([x, y, z, a[3]]).at(a[4]),
            Self::Random => random(x, y),
            Self::Noise => noise(x, y),
        }
    }
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Uniform in [0, 1) from `seed` and an integer `i`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn hash01(seed: f64, i: f64) -> f64 {
    // `+ 0.0` folds -0 into 0, so both seeds give the same stream.
    let s = splitmix64((seed + 0.0).to_bits());
    let h = splitmix64(s ^ (i as i64 as u64));
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// `random(seed)` on frame `n`: uniform in [0, 1), the same on every render.
pub fn random(seed: f64, n: f64) -> f64 {
    hash01(seed, n.round())
}

/// `noise(seed, x)`: value noise in [-1, 1], smooth in `x` (lattice values at integers, eased
/// with the quintic fade between them).
pub fn noise(seed: f64, x: f64) -> f64 {
    let i = x.floor();
    let u = x - i;
    let f = u * u * u * (u * (u * 6.0 - 15.0) + 10.0);
    let (a, b) = (
        hash01(seed, i) * 2.0 - 1.0,
        hash01(seed, i + 1.0) * 2.0 - 1.0,
    );
    a + (b - a) * f
}

/// A function's op and its accepted argument counts.
fn function(name: &str) -> Option<(F, usize, usize)> {
    let many = MAX_ARGS;
    Some(match name {
        "lt" => (F::Lt, 2, 2),
        "lte" => (F::Le, 2, 2),
        "gt" => (F::Gt, 2, 2),
        "gte" => (F::Ge, 2, 2),
        "eq" => (F::Eq, 2, 2),
        "not" => (F::Not, 1, 1),
        "between" => (F::Between, 3, 3),
        "clip" => (F::Clip, 3, 3),
        "min" => (F::Min, 2, many),
        "max" => (F::Max, 2, many),
        "abs" => (F::Abs, 1, 1),
        "floor" => (F::Floor, 1, 1),
        "ceil" => (F::Ceil, 1, 1),
        "round" => (F::Round, 1, 1),
        "trunc" => (F::Trunc, 1, 1),
        "mod" => (F::Mod, 2, 2),
        "sqrt" => (F::Sqrt, 1, 1),
        "pow" => (F::Pow, 2, 2),
        "exp" => (F::Exp, 1, 1),
        "log" => (F::Log, 1, 1),
        "sin" => (F::Sin, 1, 1),
        "cos" => (F::Cos, 1, 1),
        "tan" => (F::Tan, 1, 1),
        "asin" => (F::Asin, 1, 1),
        "acos" => (F::Acos, 1, 1),
        "atan" => (F::Atan, 1, 1),
        "atan2" => (F::Atan2, 2, 2),
        "hypot" => (F::Hypot, 2, 2),
        "sign" | "sgn" => (F::Sign, 1, 1),
        "squish" => (F::Squish, 1, 1),
        "gauss" => (F::Gauss, 1, 1),
        "lerp" => (F::Lerp, 3, 3),
        "smoothstep" => (F::Smoothstep, 3, 3),
        "ease" => (F::Ease(Ease::Ease), 1, 1),
        "ease_in" => (F::Ease(Ease::In), 1, 1),
        "ease_out" => (F::Ease(Ease::Out), 1, 1),
        "ease_in_out" => (F::Ease(Ease::InOut), 1, 1),
        "back" => (F::Ease(Ease::Back), 1, 1),
        "bezier" => (F::Bezier, 5, 5),
        "noise" => (F::Noise, 2, 2),
        _ => return None,
    })
}

/// Names an expression can't use for anything else (`vars` may not shadow them).
pub fn is_builtin_name(name: &str) -> bool {
    matches!(name, "PI" | "E" | "PHI" | "if" | "ifnot" | "random")
        || function(name).is_some()
        || super::Builtin::from_name(name).is_some()
}

/// The resolved, folded tree.
#[derive(Debug, Clone, PartialEq)]
enum Ir {
    Num(f64),
    Slot(u16),
    Op(F, Vec<Ir>),
}

#[allow(clippy::too_many_lines)]
fn resolve(n: &Node, scope: &Scope) -> Res<Ir> {
    let ir = match n {
        Node::Num(v) => Ir::Num(*v),
        Node::Var(name, col) => match name.as_str() {
            "PI" => Ir::Num(PI),
            "E" => Ir::Num(E),
            "PHI" => Ir::Num(PHI),
            _ => Ir::Slot(
                scope
                    .slot(name)
                    .ok_or_else(|| ExprError::new(*col, scope.unknown(name)))?,
            ),
        },
        Node::Call(name, col, args) => {
            let a = args
                .iter()
                .map(|a| resolve(a, scope))
                .collect::<Res<Vec<_>>>()?;
            let count = |lo: usize, hi: usize| {
                if a.len() < lo || a.len() > hi {
                    let want = match (lo, hi) {
                        (l, h) if l == h => format!("{l}"),
                        (l, MAX_ARGS) => format!("{l} or more"),
                        (l, h) => format!("{l} or {h}"),
                    };
                    Err(ExprError::new(
                        *col,
                        format!("{name}() takes {want} arguments, not {}", a.len()),
                    ))
                } else {
                    Ok(())
                }
            };
            let op = match name.as_str() {
                "neg" => F::Neg,
                "!" => F::Not,
                "+" => F::Add,
                "-" => F::Sub,
                "*" => F::Mul,
                "/" => F::Div,
                "%" => F::Mod,
                "^" => F::Pow,
                "<" => F::Lt,
                "<=" => F::Le,
                ">" => F::Gt,
                ">=" => F::Ge,
                "==" => F::Eq,
                "!=" => F::Ne,
                "&&" => F::And,
                "||" => F::Or,
                "?:" => F::Select,
                "if" | "ifnot" => {
                    count(2, 3)?;
                    let mut a = a.into_iter();
                    let (c, x) = (a.next(), a.next());
                    let y = a.next().unwrap_or(Ir::Num(0.0));
                    let (c, x) = (c.unwrap_or(Ir::Num(0.0)), x.unwrap_or(Ir::Num(0.0)));
                    let args = if name == "if" {
                        vec![c, x, y]
                    } else {
                        vec![c, y, x]
                    };
                    return Ok(fold(F::Select, args));
                }
                "random" => {
                    count(1, 1)?;
                    let n = scope.slot("n").ok_or_else(|| {
                        ExprError::new(
                            *col,
                            "random() needs the frame index `n`, which isn't known here",
                        )
                    })?;
                    let mut a = a;
                    a.push(Ir::Slot(n));
                    return Ok(Ir::Op(F::Random, a));
                }
                _ => {
                    let (f, lo, hi) = function(name).ok_or_else(|| {
                        ExprError::new(*col, format!("unknown function `{name}`"))
                    })?;
                    count(lo, hi)?;
                    f
                }
            };
            fold(op, a)
        }
    };
    Ok(ir)
}

fn fold(f: F, args: Vec<Ir>) -> Ir {
    let consts: Option<Vec<f64>> = args
        .iter()
        .map(|a| match a {
            Ir::Num(v) => Some(*v),
            _ => None,
        })
        .collect();
    match consts {
        Some(c) => Ir::Num(f.apply(&c)),
        None => Ir::Op(f, args),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Push(f64),
    Load(u16),
    Apply(F, u8),
}

/// A compiled expression: evaluates in about a hundred nanoseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    src: String,
    ops: Vec<Op>,
    depth: usize,
}

fn emit(ir: &Ir, ops: &mut Vec<Op>, sp: &mut usize, max: &mut usize) {
    match ir {
        Ir::Num(v) => {
            ops.push(Op::Push(*v));
            *sp += 1;
        }
        Ir::Slot(s) => {
            ops.push(Op::Load(*s));
            *sp += 1;
        }
        Ir::Op(f, a) => {
            for x in a {
                emit(x, ops, sp, max);
            }
            // The parser caps calls at `MAX_ARGS` (255) arguments.
            let n = u8::try_from(a.len()).expect("at most MAX_ARGS arguments");
            ops.push(Op::Apply(*f, n));
            *sp = *sp + 1 - a.len();
        }
    }
    *max = (*max).max(*sp);
}

impl Compiled {
    /// Compiles `src` against `scope`. An expression that folds to a non-finite constant
    /// (`1/0`, `log(-1)`) is an error.
    pub fn new(src: &str, scope: &Scope) -> Res<Self> {
        let ir = resolve(&parse(src)?, scope)?;
        if let Ir::Num(v) = ir
            && !v.is_finite()
        {
            return Err(ExprError::new(1, format!("always {v}: not finite")));
        }
        let (mut ops, mut sp, mut depth) = (Vec::new(), 0, 0);
        emit(&ir, &mut ops, &mut sp, &mut depth);
        Ok(Self {
            src: src.to_owned(),
            ops,
            depth,
        })
    }

    /// A constant (no source text).
    pub fn constant(v: f64) -> Self {
        Self {
            src: String::new(),
            ops: vec![Op::Push(v)],
            depth: 1,
        }
    }

    /// The value if it folded to a constant.
    pub fn as_const(&self) -> Option<f64> {
        match self.ops[..] {
            [Op::Push(v)] => Some(v),
            _ => None,
        }
    }

    pub fn source(&self) -> &str {
        &self.src
    }

    /// The value on `frame` (may be non-finite: [`super::Track::eval`] checks).
    pub fn eval(&self, frame: &Frame) -> f64 {
        if let [Op::Push(v)] = self.ops[..] {
            return v;
        }
        let mut st: Vec<f64> = Vec::with_capacity(self.depth);
        for op in &self.ops {
            match *op {
                Op::Push(v) => st.push(v),
                Op::Load(s) => st.push(frame.get(s)),
                Op::Apply(f, n) => {
                    let at = st.len() - usize::from(n);
                    let v = f.apply(&st[at..]);
                    st.truncate(at);
                    st.push(v);
                }
            }
        }
        st.pop().unwrap_or(f64::NAN)
    }
}
