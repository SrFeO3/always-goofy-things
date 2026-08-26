//! Deterministic expression evaluator: calc.
//!
//! The LLM sends a BATCH of expression strings and receives one result
//! element per expression, in the same order. Expressions are pure: no
//! assignment, no loops, no file or network access. Arithmetic follows
//! IEEE 754 double; integer literals are kept as exact `i64` values so that
//! epoch (ns) conversions stay exact even beyond 2^53.
//!
//! The reasoning loop (`run_reasoning_loop`) supplies a `CalcLedger`; this
//! module assigns `calc_id`s (C-0001, C-0002, ...) and appends one structured
//! record per expression to the ledger: todo modes append under
//! `<workspace>/artifacts/`, other modes to the session data dir (same root
//! as the session JSONL files).

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, percent_encode};
use serde::Serialize;
use serde_json::{Map, Number, Value, json};

use crate::persistence;
use crate::tools;

// ---------------------------------------------------------------------------
// Limits (spec: constraints)
// ---------------------------------------------------------------------------

/// Max expressions per call.
const MAX_EXPRESSIONS_PER_CALL: usize = 50;
/// Max characters per expression.
const MAX_EXPRESSION_CHARS: usize = 500;
/// Max expression nesting (parentheses, unary signs, call arguments) and
/// AST depth (operator chains). Enforced independently of
/// MAX_EXPRESSION_CHARS: parse recursion is capped by a counter, eval
/// recursion by an iterative AST-depth check, so raising the length limit
/// can never turn either into a stack overflow.
const MAX_EXPR_NESTING: usize = 100;
/// Whole-call wall-clock budget.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Error codes (spec: error codes)
// ---------------------------------------------------------------------------

const CODE_PARSE_ERROR: &str = "PARSE_ERROR";
const CODE_UNKNOWN_FUNCTION: &str = "UNKNOWN_FUNCTION";
const CODE_INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
const CODE_VALUE_OUT_OF_RANGE: &str = "VALUE_OUT_OF_RANGE";
const CODE_LIMIT_EXCEEDED: &str = "LIMIT_EXCEEDED";

// ---------------------------------------------------------------------------
// Values, errors, parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Val {
    /// Exact integer literal (also the carrier for epoch values).
    Int(i64),
    /// IEEE 754 double (spec: precision).
    Float(f64),
    Str(String),
    /// Arbitrary JSON (json_get results).
    Json(Value),
}

#[derive(Debug, Clone)]
struct CalcError {
    code: &'static str,
    message: String,
}

impl CalcError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    fn parse(message: impl Into<String>) -> Self {
        Self::new(CODE_PARSE_ERROR, message)
    }
    fn invalid_arg(message: impl Into<String>) -> Self {
        Self::new(CODE_INVALID_ARGUMENT, message)
    }
    fn out_of_range(message: impl Into<String>) -> Self {
        Self::new(CODE_VALUE_OUT_OF_RANGE, message)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),
    Char(char), // one of + - * / % ( ) ,
}

fn lex(input: &str) -> Result<Vec<Tok>, CalcError> {
    let mut toks = Vec::new();
    let mut it = input.chars().peekable();
    while let Some(&c) = it.peek() {
        match c {
            c if c.is_whitespace() => {
                it.next();
            }
            '+' | '-' | '*' | '/' | '%' | '(' | ')' | ',' => {
                toks.push(Tok::Char(c));
                it.next();
            }
            '\'' | '"' => {
                let quote = c;
                it.next();
                let mut s = String::new();
                loop {
                    match it.next() {
                        Some('\\') => match it.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('r') => s.push('\r'),
                            Some('\\') => s.push('\\'),
                            Some(q) if q == quote => s.push(q),
                            // Lenient on unknown escapes: keep them verbatim.
                            Some(other) => {
                                s.push('\\');
                                s.push(other);
                            }
                            None => return Err(CalcError::parse("unterminated string literal")),
                        },
                        Some(q) if q == quote => break,
                        Some(ch) => s.push(ch),
                        None => return Err(CalcError::parse("unterminated string literal")),
                    }
                }
                toks.push(Tok::Str(s));
            }
            c if c.is_ascii_digit() => {
                let mut num = String::new();
                num.push(c);
                it.next();
                while let Some(&ch) = it.peek() {
                    if ch.is_ascii_digit()
                        || (ch == '.' && !num.contains('.'))
                        || ((ch == 'e' || ch == 'E') && !num.contains('e') && !num.contains('E'))
                    {
                        num.push(ch);
                        it.next();
                        // Optional exponent sign follows 'e'/'E'.
                        if (ch == 'e' || ch == 'E')
                            && let Some(&sgn) = it.peek()
                            && (sgn == '+' || sgn == '-')
                        {
                            num.push(sgn);
                            it.next();
                        }
                    } else {
                        break;
                    }
                }
                let tok = if !num.contains(['.', 'e', 'E']) {
                    match num.parse::<i64>() {
                        Ok(v) => Tok::Int(v),
                        // Beyond i64 there is no exact representation in f64
                        // (2^53), so reject instead of silently rounding
                        // (spec: integer literals are kept exact).
                        Err(_) => {
                            return Err(CalcError::new(
                                CODE_VALUE_OUT_OF_RANGE,
                                "integer literal out of range",
                            ));
                        }
                    }
                } else {
                    Tok::Float(parse_f64_literal(&num)?)
                };
                toks.push(tok);
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut ident = String::new();
                while let Some(&ch) = it.peek() {
                    if ch.is_alphanumeric() || ch == '_' {
                        ident.push(ch);
                        it.next();
                    } else {
                        break;
                    }
                }
                toks.push(Tok::Ident(ident));
            }
            other => {
                return Err(CalcError::parse(format!(
                    "unexpected character '{}'",
                    other
                )));
            }
        }
    }
    Ok(toks)
}

/// Parse a numeric literal as f64, rejecting values that overflow to
/// infinity or underflow to zero-invalid ranges (`1e999` parses to `inf` in
/// Rust's from_str; the evaluator must never hold a non-finite literal).
fn parse_f64_literal(num: &str) -> Result<f64, CalcError> {
    let f = num
        .parse::<f64>()
        .map_err(|_| CalcError::parse("invalid number"))?;
    if f.is_finite() {
        Ok(f)
    } else {
        Err(CalcError::parse("number out of range"))
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Lit(Val),
    Call {
        name: String,
        args: Vec<Expr>,
    },
    Neg(Box<Expr>),
    Bin {
        op: char,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    nesting: usize,
}

impl Parser {
    fn peek(&self) -> Option<Tok> {
        self.toks.get(self.pos).cloned()
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_expr(&mut self) -> Result<Expr, CalcError> {
        self.nested(|s| s.parse_add())
    }

    /// Run `f` with the nesting counter bumped; reject expressions nested
    /// deeper than MAX_EXPR_NESTING. The bound keeps parse recursion
    /// explicitly capped, independently of the character limit.
    fn nested(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<Expr, CalcError>,
    ) -> Result<Expr, CalcError> {
        self.nesting += 1;
        let result = if self.nesting > MAX_EXPR_NESTING {
            Err(nesting_too_deep())
        } else {
            f(self)
        };
        self.nesting -= 1;
        result
    }

    fn parse_add(&mut self) -> Result<Expr, CalcError> {
        let mut lhs = self.parse_mul()?;
        while let Some(Tok::Char(op)) = self.peek()
            && (op == '+' || op == '-')
        {
            self.next();
            let rhs = self.parse_mul()?;
            lhs = Expr::Bin {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, CalcError> {
        let mut lhs = self.parse_unary()?;
        while let Some(Tok::Char(op)) = self.peek()
            && (op == '*' || op == '/' || op == '%')
        {
            self.next();
            let rhs = self.parse_unary()?;
            lhs = Expr::Bin {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, CalcError> {
        match self.peek() {
            Some(Tok::Char('-')) => {
                self.next();
                let e = self.nested(|s| s.parse_unary())?;
                Ok(Expr::Neg(Box::new(e)))
            }
            Some(Tok::Char('+')) => {
                self.next();
                self.nested(|s| s.parse_unary())
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, CalcError> {
        match self.next() {
            Some(Tok::Int(i)) => Ok(Expr::Lit(Val::Int(i))),
            Some(Tok::Float(f)) => Ok(Expr::Lit(Val::Float(f))),
            Some(Tok::Str(s)) => Ok(Expr::Lit(Val::Str(s))),
            Some(Tok::Ident(name)) => {
                if self.peek() == Some(Tok::Char('(')) {
                    self.next();
                    let mut args = Vec::new();
                    if self.peek() != Some(Tok::Char(')')) {
                        loop {
                            args.push(self.parse_expr()?);
                            match self.peek() {
                                Some(Tok::Char(',')) => {
                                    self.next();
                                }
                                Some(Tok::Char(')')) => break,
                                _ => {
                                    return Err(CalcError::parse(
                                        "expected ',' or ')' in argument list",
                                    ));
                                }
                            }
                        }
                    }
                    if self.next() != Some(Tok::Char(')')) {
                        return Err(CalcError::parse("expected ')'"));
                    }
                    Ok(Expr::Call { name, args })
                } else {
                    Err(CalcError::parse(format!(
                        "expected '(' after function name '{}'",
                        name
                    )))
                }
            }
            Some(Tok::Char('(')) => {
                let e = self.parse_expr()?;
                if self.next() != Some(Tok::Char(')')) {
                    return Err(CalcError::parse("expected ')'"));
                }
                Ok(e)
            }
            Some(other) => Err(CalcError::parse(format!("unexpected token {:?}", other))),
            None => Err(CalcError::parse("unexpected end of expression")),
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Result of one function call: the value plus an optional `unit` (spec:
/// response format - unit-carrying conversions attach `"unit"` to the
/// element).
struct FnResult {
    value: Val,
    unit: Option<String>,
}

fn eval(expr: &Expr) -> Result<Val, CalcError> {
    Ok(eval_full(expr)?.0)
}

fn eval_full(expr: &Expr) -> Result<(Val, Option<String>), CalcError> {
    match expr {
        Expr::Lit(v) => Ok((v.clone(), None)),
        Expr::Neg(e) => {
            let (v, _) = eval_full(e)?;
            let neg = match v {
                Val::Int(i) => i
                    .checked_neg()
                    .map(Val::Int)
                    .ok_or_else(|| CalcError::out_of_range("integer overflow"))?,
                Val::Float(f) => Val::Float(-f),
                _ => return Err(CalcError::invalid_arg("expected a number, got a string")),
            };
            Ok((neg, None))
        }
        Expr::Bin { op, lhs, rhs } => {
            let (a, _) = eval_full(lhs)?;
            let (b, _) = eval_full(rhs)?;
            Ok((arith(*op, a, b)?, None))
        }
        Expr::Call { name, args } => {
            let mut vals = Vec::with_capacity(args.len());
            for arg in args {
                vals.push(eval(arg)?);
            }
            let fr = call_function(name, &vals)?;
            Ok((fr.value, fr.unit))
        }
    }
}

fn to_float(v: &Val) -> Result<f64, CalcError> {
    match v {
        Val::Int(i) => Ok(*i as f64),
        Val::Float(f) => Ok(*f),
        _ => Err(CalcError::invalid_arg("expected a number, got a string")),
    }
}

/// Exact integer required (epoch conversions): integers pass through,
/// integral doubles within i64 range are accepted, everything else fails.
fn expect_int(name: &str, param: &str, v: &Val) -> Result<i64, CalcError> {
    match v {
        Val::Int(i) => Ok(*i),
        Val::Float(f)
            if f.is_finite()
                && f.fract() == 0.0
                && *f >= i64::MIN as f64
                && *f < 9223372036854775808.0 /* 2^63 */ =>
        {
            Ok(*f as i64)
        }
        Val::Float(_) => Err(CalcError::invalid_arg(format!(
            "{}: '{}' expected an integer, got a fractional number",
            name, param
        ))),
        _ => Err(CalcError::invalid_arg(format!(
            "{}: '{}' expected a number, got a string",
            name, param
        ))),
    }
}

fn expect_str<'a>(name: &str, param: &str, v: &'a Val) -> Result<&'a str, CalcError> {
    match v {
        Val::Str(s) => Ok(s),
        _ => Err(CalcError::invalid_arg(format!(
            "{}: '{}' expected a string, got a number",
            name, param
        ))),
    }
}

fn arity(name: &str, args: &[Val], min: usize, max: usize) -> Result<(), CalcError> {
    let expected = if min == max {
        min.to_string()
    } else {
        format!("{}-{}", min, max)
    };
    if args.len() < min || args.len() > max {
        return Err(CalcError::invalid_arg(format!(
            "{} expects {} argument(s), got {}",
            name,
            expected,
            args.len()
        )));
    }
    Ok(())
}

fn arith(op: char, a: Val, b: Val) -> Result<Val, CalcError> {
    use Val::{Float, Int};
    // Exact integer path (keeps epoch-scale literals exact when no float
    // enters the operation); everything else follows IEEE 754 double.
    match (op, a, b) {
        ('+', Int(x), Int(y)) => int_op(x, y, |x, y| x.checked_add(y)),
        ('-', Int(x), Int(y)) => int_op(x, y, |x, y| x.checked_sub(y)),
        ('*', Int(x), Int(y)) => int_op(x, y, |x, y| x.checked_mul(y)),
        ('%', Int(x), Int(y)) => {
            if y == 0 {
                return Err(CalcError::out_of_range("division by zero"));
            }
            int_op(x, y, |x, y| x.checked_rem(y))
        }
        ('/', Int(x), Int(y)) => {
            if y == 0 {
                return Err(CalcError::out_of_range("division by zero"));
            }
            Ok(Float(x as f64 / y as f64))
        }
        (op, x, y) => {
            let fx = to_float(&x)?;
            let fy = to_float(&y)?;
            let r = match op {
                '+' => fx + fy,
                '-' => fx - fy,
                '*' => fx * fy,
                '/' => {
                    if fy == 0.0 {
                        return Err(CalcError::out_of_range("division by zero"));
                    }
                    fx / fy
                }
                '%' => {
                    if fy == 0.0 {
                        return Err(CalcError::out_of_range("division by zero"));
                    }
                    fx % fy
                }
                _ => unreachable!("arithmetic operator validated by the parser"),
            };
            if !r.is_finite() {
                return Err(CalcError::out_of_range(
                    "result is not finite (overflow or division by zero)",
                ));
            }
            Ok(Float(r))
        }
    }
}

fn int_op(x: i64, y: i64, f: impl Fn(i64, i64) -> Option<i64>) -> Result<Val, CalcError> {
    f(x, y)
        .map(Val::Int)
        .ok_or_else(|| CalcError::out_of_range("integer overflow"))
}

// ---------------------------------------------------------------------------
// Function catalog: initial scope (spec)
// ---------------------------------------------------------------------------

fn call_function(name: &str, args: &[Val]) -> Result<FnResult, CalcError> {
    let unit_of = |u: &str| Some(u.to_string());
    let no_unit = None;
    match name {
        // -- time & calendar -------------------------------------------------
        "epoch_ns_to_utc" => {
            arity(name, args, 1, 1)?;
            let ns = expect_int(name, "ns", &args[0])?;
            epoch_to_utc(
                ns.div_euclid(1_000_000_000),
                ns.rem_euclid(1_000_000_000) as u32,
            )
            .map(|v| FnResult {
                value: v,
                unit: unit_of("UTC"),
            })
        }
        "epoch_s_to_utc" => {
            arity(name, args, 1, 1)?;
            let s = expect_int(name, "seconds", &args[0])?;
            epoch_to_utc(s, 0).map(|v| FnResult {
                value: v,
                unit: unit_of("UTC"),
            })
        }
        "epoch_ms_to_utc" => {
            arity(name, args, 1, 1)?;
            let ms = expect_int(name, "ms", &args[0])?;
            epoch_to_utc(
                ms.div_euclid(1000),
                (ms.rem_euclid(1000) * 1_000_000) as u32,
            )
            .map(|v| FnResult {
                value: v,
                unit: unit_of("UTC"),
            })
        }
        "utc_to_epoch" => {
            arity(name, args, 1, 1)?;
            let s = expect_str(name, "iso", &args[0])?;
            let (secs, nanos) = parse_datetime_utc(s)?;
            let value = if nanos == 0 {
                Val::Int(secs)
            } else {
                Val::Float(secs as f64 + nanos as f64 / 1e9)
            };
            Ok(FnResult {
                value,
                unit: unit_of("s"),
            })
        }
        "duration_between" => {
            arity(name, args, 2, 2)?;
            let from = parse_datetime_utc(expect_str(name, "from", &args[0])?)?;
            let to = parse_datetime_utc(expect_str(name, "to", &args[1])?)?;
            let from_ns = (from.0 as i128) * 1_000_000_000 + from.1 as i128;
            let to_ns = (to.0 as i128) * 1_000_000_000 + to.1 as i128;
            let diff_ns = to_ns - from_ns;
            let value = if diff_ns % 1_000_000_000 == 0 {
                Val::Int((diff_ns / 1_000_000_000) as i64)
            } else {
                Val::Float(diff_ns as f64 / 1e9)
            };
            Ok(FnResult {
                value,
                unit: unit_of("s"),
            })
        }
        "tz_convert" => {
            arity(name, args, 2, 2)?;
            let dt_str = expect_str(name, "datetime", &args[0])?;
            let zone = expect_str(name, "zone", &args[1])?;
            let (secs, nanos) = parse_datetime_utc(dt_str)?;
            let offset = parse_zone(zone)?;
            let dt = DateTime::<Utc>::from_timestamp(secs, nanos)
                .ok_or_else(|| CalcError::out_of_range("datetime out of supported range"))?;
            let local = dt.with_timezone(&offset);
            let (text, unit) = if offset.local_minus_utc() == 0 {
                (
                    local.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                    "UTC".to_string(),
                )
            } else {
                (
                    local.format("%Y-%m-%d %H:%M:%S%:z").to_string(),
                    zone.to_string(),
                )
            };
            Ok(FnResult {
                value: Val::Str(text),
                unit: Some(unit),
            })
        }

        // -- arithmetic, units, ratios --------------------------------------
        "percent" => {
            arity(name, args, 2, 2)?;
            let part = to_float(&args[0])?;
            let whole = to_float(&args[1])?;
            if whole == 0.0 {
                return Err(CalcError::out_of_range("division by zero"));
            }
            let r = part / whole * 100.0;
            if !r.is_finite() {
                return Err(CalcError::out_of_range("result is not finite"));
            }
            Ok(FnResult {
                value: Val::Float(r),
                unit: no_unit,
            })
        }
        "rate" => {
            arity(name, args, 2, 2)?;
            let count = to_float(&args[0])?;
            let seconds = to_float(&args[1])?;
            if seconds == 0.0 {
                return Err(CalcError::out_of_range("division by zero"));
            }
            let r = count / seconds;
            if !r.is_finite() {
                return Err(CalcError::out_of_range("result is not finite"));
            }
            Ok(FnResult {
                value: Val::Float(r),
                unit: no_unit,
            })
        }
        "round" => {
            arity(name, args, 1, 2)?;
            let n = to_float(&args[0])?;
            let digits = if args.len() == 2 {
                expect_int(name, "digits", &args[1])?
            } else {
                0
            };
            if !(0..=15).contains(&digits) {
                return Err(CalcError::invalid_arg(format!(
                    "{}: 'digits' must be between 0 and 15, got {}",
                    name, digits
                )));
            }
            let scale = 10f64.powi(digits as i32);
            let r = (n * scale).round() / scale;
            if !r.is_finite() {
                return Err(CalcError::out_of_range("result is not finite"));
            }
            Ok(FnResult {
                value: Val::Float(r),
                unit: no_unit,
            })
        }
        "sum" | "avg" => {
            arity(name, args, 1, usize::MAX)?;
            let mut total = 0.0f64;
            for v in args {
                total += to_float(v)?;
            }
            let r = if name == "avg" {
                total / args.len() as f64
            } else {
                total
            };
            if !r.is_finite() {
                return Err(CalcError::out_of_range("result is not finite"));
            }
            Ok(FnResult {
                value: Val::Float(r),
                unit: no_unit,
            })
        }
        "bytes_to_human" => {
            arity(name, args, 1, 1)?;
            let n = to_float(&args[0])?;
            if n < 0.0 || !n.is_finite() {
                return Err(CalcError::out_of_range(
                    "byte count must be a non-negative finite number",
                ));
            }
            const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
            let (mut v, mut u) = (n, 0);
            while v >= 1024.0 && u < UNITS.len() - 1 {
                v /= 1024.0;
                u += 1;
            }
            // Integer formatting only while the value is exactly representable
            // (below 2^53); larger values would saturate the i64 cast.
            let text = if v.fract() == 0.0 && v < 9_007_199_254_740_992.0 {
                format!("{} {}", v as i64, UNITS[u])
            } else {
                format!("{:.1} {}", v, UNITS[u])
            };
            Ok(FnResult {
                value: Val::Str(text),
                unit: unit_of(UNITS[u]),
            })
        }
        "bytes_unit" => {
            arity(name, args, 2, 2)?;
            let n = to_float(&args[0])?;
            if n < 0.0 || !n.is_finite() {
                return Err(CalcError::out_of_range(
                    "byte count must be a non-negative finite number",
                ));
            }
            let unit = expect_str(name, "unit", &args[1])?;
            let (canonical, idx) = match unit.trim().to_ascii_uppercase().as_str() {
                "B" => ("B", 0),
                "KB" | "KIB" => ("KiB", 1),
                "MB" | "MIB" => ("MiB", 2),
                "GB" | "GIB" => ("GiB", 3),
                "TB" | "TIB" => ("TiB", 4),
                "PB" | "PIB" => ("PiB", 5),
                _ => {
                    return Err(CalcError::invalid_arg(format!(
                        "unknown byte unit '{}'; supported: B, KB/KiB, MB/MiB, GB/GiB, TB/TiB, PB/PiB",
                        unit
                    )));
                }
            };
            let r = n / 1024f64.powi(idx);
            Ok(FnResult {
                value: Val::Float(r),
                unit: unit_of(canonical),
            })
        }

        // -- string / decode -------------------------------------------------
        "base64_encode" => {
            arity(name, args, 1, 1)?;
            let s = expect_str(name, "value", &args[0])?;
            Ok(FnResult {
                value: Val::Str(STANDARD.encode(s.as_bytes())),
                unit: no_unit,
            })
        }
        "base64_decode" => {
            arity(name, args, 1, 1)?;
            let s = expect_str(name, "value", &args[0])?;
            Ok(decode_bytes(base64_decode_any(s)?))
        }
        "hex_encode" => {
            arity(name, args, 1, 1)?;
            let s = expect_str(name, "value", &args[0])?;
            // formatting through bytes keeps the ASCII fast path simple
            let mut out = String::with_capacity(s.len() * 2);
            for b in s.as_bytes() {
                out.push_str(&format!("{:02x}", b));
            }
            Ok(FnResult {
                value: Val::Str(out),
                unit: no_unit,
            })
        }
        "hex_decode" => {
            arity(name, args, 1, 1)?;
            let s = expect_str(name, "value", &args[0])?;
            Ok(decode_bytes(hex_decode_any(s)?))
        }
        "url_encode" => {
            arity(name, args, 1, 1)?;
            let s = expect_str(name, "value", &args[0])?;
            let encoded = percent_encode(s.as_bytes(), NON_ALPHANUMERIC).to_string();
            Ok(FnResult {
                value: Val::Str(encoded),
                unit: no_unit,
            })
        }
        "url_decode" => {
            arity(name, args, 1, 1)?;
            let s = expect_str(name, "value", &args[0])?;
            // '+' -> ' ' follows form-urlencoded semantics for HTTP payloads.
            let plus_fixed = s.replace('+', " ");
            let decoded: Vec<u8> = percent_decode_str(&plus_fixed).collect();
            Ok(decode_bytes(decoded))
        }
        "json_get" => {
            arity(name, args, 2, 2)?;
            let js = expect_str(name, "json", &args[0])?;
            let path = expect_str(name, "path", &args[1])?;
            let parsed: Value = serde_json::from_str(js)
                .map_err(|e| CalcError::invalid_arg(format!("invalid JSON: {}", e)))?;
            let segs = parse_json_path(path)?;
            let mut cur = parsed;
            for seg in segs {
                cur = match (&cur, seg) {
                    (Value::Object(m), Seg::Key(k)) => m.get(&k).cloned().ok_or_else(|| {
                        CalcError::invalid_arg(format!(
                            "JSON path '{}' not found: missing key '{}'",
                            path, k
                        ))
                    })?,
                    (Value::Array(a), Seg::Index(i)) => a.get(i).cloned().ok_or_else(|| {
                        CalcError::invalid_arg(format!(
                            "JSON path '{}' not found: index {} out of range",
                            path, i
                        ))
                    })?,
                    _ => {
                        return Err(CalcError::invalid_arg(format!(
                            "JSON path '{}' cannot navigate through a non-object/array value",
                            path
                        )));
                    }
                };
            }
            Ok(FnResult {
                value: Val::Json(cur),
                unit: no_unit,
            })
        }
        "normalize" => {
            arity(name, args, 1, 1)?;
            let s = expect_str(name, "value", &args[0])?;
            // Normalization (notation unification): trim + collapse all
            // whitespace runs + Unicode lowercase.
            let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
            Ok(FnResult {
                value: Val::Str(collapsed.to_lowercase()),
                unit: no_unit,
            })
        }
        _ => Err(CalcError {
            code: CODE_UNKNOWN_FUNCTION,
            message: format!("unknown function '{}'", name),
        }),
    }
}

/// Parameter names per function: used to build the ledger `inputs` record
/// (spec: number ledger) when the expression is a top-level function call.
fn param_names(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "percent" => &["part", "whole"],
        "rate" => &["count", "seconds"],
        "round" => &["value", "digits"],
        "sum" | "avg" => &["values"],
        "epoch_ns_to_utc" => &["ns"],
        "epoch_s_to_utc" => &["seconds"],
        "epoch_ms_to_utc" => &["ms"],
        "utc_to_epoch" => &["iso"],
        "duration_between" => &["from", "to"],
        "tz_convert" => &["datetime", "zone"],
        "bytes_to_human" => &["bytes"],
        "bytes_unit" => &["bytes", "unit"],
        "base64_encode" | "base64_decode" | "hex_encode" | "hex_decode" | "url_encode"
        | "url_decode" | "normalize" => &["value"],
        "json_get" => &["json", "path"],
        _ => return None,
    })
}

fn epoch_to_utc(secs: i64, nanos: u32) -> Result<Val, CalcError> {
    let dt = DateTime::<Utc>::from_timestamp(secs, nanos)
        .ok_or_else(|| CalcError::out_of_range("epoch out of supported range"))?;
    // Spec: timestamps are always rendered as `YYYY-MM-DD HH:MM:SS UTC`.
    Ok(Val::Str(dt.format("%Y-%m-%d %H:%M:%S UTC").to_string()))
}

/// Parse a calendar/clock string to (whole seconds, sub-second nanos) in UTC.
/// Accepted forms: RFC 3339 with offset or `Z`, naive `YYYY-MM-DD[ |T]HH:MM:SS`
/// (assumed UTC, optional fractional seconds), a trailing "UTC" suffix, and
/// date-only `YYYY-MM-DD` (midnight UTC).
fn parse_datetime_utc(s: &str) -> Result<(i64, u32), CalcError> {
    let bad = || {
        CalcError::invalid_arg(format!(
            "invalid datetime '{}': expected ISO 8601 / RFC 3339 such as '2016-08-24 16:49:24 UTC'",
            s
        ))
    };
    let raw = s.trim();
    let src = raw.strip_suffix("UTC").map(str::trim).unwrap_or(raw);
    let parts: Vec<&str> = src.split_whitespace().collect();
    let candidate = match parts.len() {
        1 => src.to_string(),
        2 => format!("{}T{}", parts[0], parts[1]),
        3 => format!("{}T{}{}", parts[0], parts[1], parts[2]),
        _ => return Err(bad()),
    };
    let has_offset = candidate.ends_with(['Z', 'z'])
        || candidate.rfind('+').is_some_and(|p| p > 10)
        || candidate.rfind('-').is_some_and(|p| p > 10);
    let with_zone = if has_offset {
        candidate.clone()
    } else {
        format!("{}Z", candidate)
    };
    let parsed: DateTime<Utc> = chrono::DateTime::parse_from_rfc3339(&with_zone)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            // RFC 3339 requires a time part; accept date-only as midnight UTC.
            if !has_offset && !candidate.contains('T') && parts.len() <= 2 {
                NaiveDate::parse_from_str(&candidate, "%Y-%m-%d")
                    .ok()
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
                    .map(|t| t.and_utc())
            } else {
                None
            }
        })
        .ok_or_else(bad)?;
    Ok((parsed.timestamp(), parsed.timestamp_subsec_nanos()))
}

/// Fixed-offset zones only: UTC / Z and `+HH[:MM]`-style offsets. Named zones
/// (e.g. Asia/Tokyo) are intentionally out of scope (no tz database in the
/// sandbox keeps the binary lean and the conversion independent of IANA
/// releases).
fn parse_zone(z: &str) -> Result<FixedOffset, CalcError> {
    let z = z.trim();
    if z.eq_ignore_ascii_case("utc") || z == "Z" || z == "z" {
        return FixedOffset::east_opt(0).ok_or_else(|| CalcError::out_of_range("invalid offset"));
    }
    let (sign, rest) = if let Some(r) = z.strip_prefix('+') {
        (1, r)
    } else if let Some(r) = z.strip_prefix('-') {
        (-1, r)
    } else {
        return Err(CalcError::invalid_arg(format!(
            "invalid timezone '{}': expected UTC or a fixed offset such as +09:00 (named zones are not supported)",
            z
        )));
    };
    // The grammar below slices by byte position; reject non-ASCII offsets
    // first so slicing always lands on char boundaries (never panics).
    if !rest.is_ascii() {
        return Err(CalcError::invalid_arg(format!(
            "invalid timezone '{}': expected an ASCII offset such as +09:00",
            z
        )));
    }
    let (h_str, m_str) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() <= 2 {
        (rest, "0")
    } else {
        rest.split_at(rest.len() - 2)
    };
    // Both parts share the same parse and error mapping.
    let parse_num = |s: &str| {
        s.parse::<i32>()
            .map_err(|_| CalcError::invalid_arg(format!("invalid timezone '{}'", z)))
    };
    let (h, m) = (parse_num(h_str)?, parse_num(m_str)?);
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return Err(CalcError::invalid_arg(format!(
            "invalid timezone '{}': offset out of range",
            z
        )));
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
        .ok_or_else(|| CalcError::out_of_range("invalid offset"))
}

/// Base64 with practical leniency: ASCII whitespace is ignored and padded /
/// unpadded standard / URL-safe alphabets are all accepted (deterministic).
fn base64_decode_any(s: &str) -> Result<Vec<u8>, CalcError> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        if let Ok(b) = engine.decode(&cleaned) {
            return Ok(b);
        }
    }
    Err(CalcError::invalid_arg("invalid base64 input"))
}

fn hex_decode_any(s: &str) -> Result<Vec<u8>, CalcError> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(CalcError::invalid_arg("hex input must have even length"));
    }
    let b = cleaned.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks_exact(2) {
        let hi = hex_val(pair[0]).ok_or_else(|| {
            CalcError::invalid_arg(format!("invalid hex character '{}'", pair[0] as char))
        })?;
        let lo = hex_val(pair[1]).ok_or_else(|| {
            CalcError::invalid_arg(format!("invalid hex character '{}'", pair[1] as char))
        })?;
        out.push(hi << 4 | lo);
    }
    Ok(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decoded bytes become a string when UTF-8, otherwise a lossless byte array
/// (binary payloads must not be corrupted by lossy conversion).
fn decode_bytes(bytes: Vec<u8>) -> FnResult {
    let value = match String::from_utf8(bytes) {
        Ok(s) => Val::Str(s),
        Err(e) => Val::Json(Value::Array(
            e.into_bytes().into_iter().map(Value::from).collect(),
        )),
    };
    FnResult { value, unit: None }
}

enum Seg {
    Key(String),
    Index(usize),
}

/// Split a dot / bracket path like `a.b[0].c` (optional leading `$`).
fn parse_json_path(path: &str) -> Result<Vec<Seg>, CalcError> {
    let mut segs = Vec::new();
    let mut chars = path.trim().chars().peekable();
    // `$` is only a root marker (before any segment); anywhere else it is
    // rejected rather than silently skipped.
    let mut at_root = true;
    while let Some(&c) = chars.peek() {
        match c {
            '.' => {
                chars.next();
            }
            '$' if at_root => {
                chars.next();
            }
            '$' => {
                return Err(CalcError::invalid_arg(format!(
                    "invalid JSON path '{}'",
                    path
                )));
            }
            '[' => {
                chars.next();
                let mut n = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        n.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if n.is_empty() || chars.next() != Some(']') {
                    return Err(CalcError::invalid_arg(format!(
                        "invalid JSON path '{}'",
                        path
                    )));
                }
                let idx = n
                    .parse::<usize>()
                    .map_err(|_| CalcError::invalid_arg(format!("invalid JSON path '{}'", path)))?;
                segs.push(Seg::Index(idx));
                at_root = false;
            }
            alnum if alnum.is_alphanumeric() || alnum == '_' || alnum == '-' || alnum == ' ' => {
                let mut key = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == ' ' {
                        key.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                segs.push(Seg::Key(key.trim().to_string()));
                at_root = false;
            }
            _ => {
                return Err(CalcError::invalid_arg(format!(
                    "invalid JSON path '{}'",
                    path
                )));
            }
        }
    }
    Ok(segs)
}

// ---------------------------------------------------------------------------
// Value to JSON
// ---------------------------------------------------------------------------

/// IEEE 754 doubles that are integral and exactly representable are emitted
/// as JSON integers (spec examples: `45600`, `50`), everything else as the
/// shortest round-tripping JSON number.
fn val_to_json(v: &Val) -> Value {
    match v {
        Val::Int(i) => json!(i),
        Val::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
                json!(*f as i64)
            } else {
                Number::from_f64(*f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
        }
        Val::Str(s) => json!(s),
        Val::Json(j) => j.clone(),
    }
}

// ---------------------------------------------------------------------------
// Batch execution (tool entry point)
// ---------------------------------------------------------------------------

/// Sequential calc_id numbering for the whole process lifetime
/// (C-0001, C-0002, ... per spec: response format).
static NEXT_CALC_ID: AtomicU64 = AtomicU64::new(1);

fn next_calc_id() -> String {
    format!("C-{:04}", NEXT_CALC_ID.fetch_add(1, Ordering::Relaxed))
}

/// Evaluate a batch of expressions and return the result JSON.
///
/// Returns a JSON array with one element per input expression, in the same
/// order (spec: response format). A single expression failing never affects the
/// others. Whole-call problems (missing / oversized batch) return a single
/// `{"error": {"code", "message"}}` object instead.
pub(crate) fn execute_calc(args: &Value, ledger: Option<&CalcLedger>) -> Value {
    let start = Instant::now();

    let Some(list) = args.get("expressions").and_then(Value::as_array) else {
        return call_error(
            CODE_INVALID_ARGUMENT,
            "'expressions' must be an array of strings, e.g. [\"1 + 2\"]",
        );
    };
    if list.is_empty() {
        return call_error(CODE_INVALID_ARGUMENT, "'expressions' must not be empty");
    }
    if list.len() > MAX_EXPRESSIONS_PER_CALL {
        return call_error(
            CODE_LIMIT_EXCEEDED,
            format!(
                "too many expressions: {} (max {})",
                list.len(),
                MAX_EXPRESSIONS_PER_CALL
            ),
        );
    }

    let mut results = Vec::with_capacity(list.len());
    for raw in list {
        let calc_id = next_calc_id();
        let expr = match raw.as_str() {
            Some(e) => e,
            None => {
                results.push(element_error(
                    &calc_id,
                    "",
                    CODE_INVALID_ARGUMENT,
                    "each expression must be a string",
                ));
                continue;
            }
        };
        if expr.chars().count() > MAX_EXPRESSION_CHARS {
            let msg = format!(
                "expression exceeds the {} character limit",
                MAX_EXPRESSION_CHARS
            );
            results.push(element_error(
                &calc_id,
                expr,
                CODE_LIMIT_EXCEEDED,
                msg.clone(),
            ));
            record_failure(ledger, &calc_id, expr, CODE_LIMIT_EXCEEDED, &msg);
            continue;
        }
        if start.elapsed() > CALL_TIMEOUT {
            let msg = format!("call time limit ({:?}) exceeded", CALL_TIMEOUT);
            results.push(element_error(
                &calc_id,
                expr,
                CODE_LIMIT_EXCEEDED,
                msg.clone(),
            ));
            record_failure(ledger, &calc_id, expr, CODE_LIMIT_EXCEEDED, &msg);
            continue;
        }

        match eval_expression(expr) {
            Ok((value, unit, inputs)) => {
                let mut el = json!({
                    "calc_id": calc_id,
                    "expression": expr,
                    "result": val_to_json(&value),
                });
                if let Some(u) = unit {
                    el["unit"] = json!(u);
                }
                results.push(el);
                record_success(ledger, &calc_id, expr, inputs, &value);
            }
            Err(e) => {
                results.push(element_error(&calc_id, expr, e.code, &e.message));
                record_failure(ledger, &calc_id, expr, e.code, &e.message);
            }
        }
    }
    Value::Array(results)
}

fn nesting_too_deep() -> CalcError {
    CalcError::new(
        CODE_LIMIT_EXCEEDED,
        format!("expression nesting exceeds {} levels", MAX_EXPR_NESTING),
    )
}

/// Maximum AST depth of `expr`, measured iteratively with an explicit
/// stack (this check itself never recurses).
fn expr_depth(expr: &Expr) -> usize {
    let mut max = 0;
    let mut stack = vec![(expr, 1usize)];
    while let Some((e, d)) = stack.pop() {
        max = max.max(d);
        match e {
            Expr::Lit(_) => {}
            Expr::Neg(inner) => stack.push((inner, d + 1)),
            Expr::Call { args, .. } => {
                for a in args {
                    stack.push((a, d + 1));
                }
            }
            Expr::Bin { lhs, rhs, .. } => {
                stack.push((lhs, d + 1));
                stack.push((rhs, d + 1));
            }
        }
    }
    max
}

/// Result of evaluating one expression: value, optional `unit`, and named
/// inputs (top-level function calls only) for the number ledger.
type Evaluated = (Val, Option<String>, Option<Vec<(String, Val)>>);

/// Parse + evaluate a single expression.
///
/// Returns the value, an optional `unit`, and - when the expression is a
/// top-level function call with known parameter names - the named inputs
/// (for the number ledger).
fn eval_expression(expr: &str) -> Result<Evaluated, CalcError> {
    let toks = lex(expr)?;
    let mut parser = Parser {
        toks,
        pos: 0,
        nesting: 0,
    };
    let ast = parser.parse_expr()?;
    if parser.pos != parser.toks.len() {
        return Err(CalcError::parse("unexpected trailing tokens"));
    }
    // Bounds the eval recursion on the AST: parens are transparent in the
    // tree and left-deep operator chains grow with expression length, so
    // the parse-time nesting counter alone does not limit them. This check
    // is iterative (never recurses) and keeps eval depth at or below
    // MAX_EXPR_NESTING regardless of MAX_EXPRESSION_CHARS.
    if expr_depth(&ast) > MAX_EXPR_NESTING {
        return Err(nesting_too_deep());
    }
    let inputs = match &ast {
        Expr::Call { name, args } => param_names(name).map(|params| {
            args.iter()
                .enumerate()
                .map(|(i, a)| {
                    let key = params.get(i).copied().unwrap_or("arg");
                    (key.to_string(), eval(a).unwrap_or(Val::Json(Value::Null)))
                })
                .collect()
        }),
        _ => None,
    };
    let (value, unit) = eval_full(&ast)?;
    Ok((value, unit, inputs))
}

fn call_error(code: &'static str, message: impl Into<String>) -> Value {
    json!({ "error": { "code": code, "message": message.into() } })
}

fn element_error(
    calc_id: &str,
    expression: &str,
    code: &'static str,
    message: impl Into<String>,
) -> Value {
    json!({
        "calc_id": calc_id,
        "expression": expression,
        "error": { "code": code, "message": message.into() }
    })
}

// ---------------------------------------------------------------------------
// Number ledger (spec: number ledger)
// ---------------------------------------------------------------------------

/// Structured ledger record appended per evaluated expression.
#[derive(Serialize)]
pub(crate) struct CalcRecord {
    pub calc_id: String,
    pub expression: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    pub source: String,
    pub recorded_at: String,
}

impl CalcRecord {
    fn ok(
        calc_id: &str,
        expression: &str,
        inputs: Option<Vec<(String, Val)>>,
        result: &Val,
    ) -> Self {
        let inputs = inputs.map(|pairs| {
            let mut m = Map::new();
            for (k, v) in pairs {
                m.insert(k, val_to_json(&v));
            }
            m
        });
        Self {
            calc_id: calc_id.to_string(),
            expression: expression.to_string(),
            inputs,
            result: Some(val_to_json(result)),
            error: None,
            source: "calc".to_string(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    fn failed(calc_id: &str, expression: &str, code: &'static str, message: &str) -> Self {
        Self {
            calc_id: calc_id.to_string(),
            expression: expression.to_string(),
            inputs: None,
            result: None,
            error: Some(json!({ "code": code, "message": message })),
            source: "calc".to_string(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Append-only destination for ledger records. Best-effort: a failing ledger
/// write never affects the tool result (the result is already in the session
/// log, which is the primary record).
pub(crate) struct CalcLedger {
    path: Option<PathBuf>,
}

impl CalcLedger {
    /// Ledger destination: todo modes append under `<workspace>/artifacts/`
    /// (spec), other modes append to the session data dir next to the
    /// session JSONL files.
    pub(crate) fn new(session_label: &str, todo_mode: u8) -> Self {
        Self {
            path: resolve_ledger_path(
                session_label,
                todo_mode,
                tools::workspace_root(),
                persistence::data_dir().as_deref(),
            ),
        }
    }

    /// Explicit destination (tests).
    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub(crate) fn record(&self, rec: &CalcRecord) {
        let Some(path) = &self.path else { return };
        let json = match serde_json::to_string(rec) {
            Ok(j) => j,
            Err(e) => {
                eprintln!(
                    "\x1b[93m[calc_ledger] failed to serialize record: {}\x1b[0m",
                    e
                );
                return;
            }
        };
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{}", json))
        {
            Ok(()) => {}
            Err(e) => eprintln!(
                "\x1b[93m[calc_ledger] failed to append to {:?}: {}\x1b[0m",
                path, e
            ),
        }
    }
}

/// Pure path resolution (unit-testable without touching globals).
/// The label becomes part of a file name, so it is sanitized to a safe
/// filename component: any char outside [A-Za-z0-9_-] is replaced and the
/// length is capped, keeping every ledger write inside the data dir.
fn resolve_ledger_path(
    session_label: &str,
    todo_mode: u8,
    workspace: &Path,
    data: Option<&Path>,
) -> Option<PathBuf> {
    if todo_mode > 0 {
        Some(workspace.join("artifacts").join("calc_ledger.jsonl"))
    } else {
        data.map(|d| {
            d.join(format!(
                "calc_ledger_{}.jsonl",
                sanitize_label(session_label)
            ))
        })
    }
}

/// Traversal-proof filename component derived from the session label.
fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

fn record_success(
    ledger: Option<&CalcLedger>,
    calc_id: &str,
    expression: &str,
    inputs: Option<Vec<(String, Val)>>,
    result: &Val,
) {
    if let Some(l) = ledger {
        l.record(&CalcRecord::ok(calc_id, expression, inputs, result));
    }
}

fn record_failure(
    ledger: Option<&CalcLedger>,
    calc_id: &str,
    expression: &str,
    code: &'static str,
    message: &str,
) {
    if let Some(l) = ledger {
        l.record(&CalcRecord::failed(calc_id, expression, code, message));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tests/tools_calc_test.rs"]
mod tests;
