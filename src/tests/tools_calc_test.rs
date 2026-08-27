//! Tests for `src/tools_calc.rs`: the deterministic `calc` tool.
//!
//! Covers the spec's validation method: boundary values (epoch 0, negative,
//! large ns), the verified failing cases from the spec appendix A (regression
//! cases that every model got wrong by mental arithmetic), decode round-trips,
//! error codes, batch limits, and the number ledger.

use super::*;
use serde_json::Value;

/// Evaluate one batch and return the result array (asserting the call
/// produced an array).
fn batch(exprs: &[&str]) -> Value {
    let args = json!({ "expressions": exprs });
    execute_calc(&args, None)
}

fn ok(v: &Value, i: usize) -> (&Value, &Value) {
    let el = &v[i];
    (el.get("expression").unwrap(), el.get("result").unwrap())
}

fn err_code(v: &Value, i: usize) -> String {
    v[i].get("error").unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// Spec examples (response format)
// ---------------------------------------------------------------------------

#[test]
fn spec_example_arithmetic_batch() {
    let v = batch(&[
        "1425 * 32",
        "(100 + 50) / 3",
        "percent(17121, 17267)",
        "round(percent(17121, 17267), 2)",
    ]);
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 4);

    // Same order as the input; expression is the verbatim copy.
    let (expr, res) = ok(&v, 0);
    assert_eq!(expr, "1425 * 32");
    assert_eq!(res.as_i64(), Some(45600));

    let (expr, res) = ok(&v, 1);
    assert_eq!(expr, "(100 + 50) / 3");
    assert_eq!(res.as_i64(), Some(50));

    let (_, res) = ok(&v, 2);
    let pct = res.as_f64().unwrap();
    assert!(
        (pct - 99.15445647767417).abs() < 1e-12,
        "percent mismatch: {}",
        pct
    );

    let (_, res) = ok(&v, 3);
    assert_eq!(res.as_f64(), Some(99.15));
}

#[test]
fn spec_example_epoch_and_unit() {
    // Spec success example: C-0019 / C-0020.
    let v = batch(&[
        "epoch_ns_to_utc(1472057364542756000)",
        "bytes_to_human(71176704)",
    ]);
    let arr = v.as_array().unwrap();
    assert_eq!(arr[0]["result"], "2016-08-24 16:49:24 UTC");
    assert_eq!(arr[0]["unit"], "UTC");
    assert_eq!(arr[1]["result"], "67.9 MiB");
    assert_eq!(arr[1]["unit"], "MiB");
}

#[test]
fn spec_appendix_a_verified_epoch_values() {
    // Spec appendix A: python-measured reference values: every model got
    // these wrong by mental arithmetic.
    let v = batch(&[
        "epoch_s_to_utc(1470832605)",
        "epoch_s_to_utc(1470865005)",
        "epoch_s_to_utc(1472057364)",
        "epoch_s_to_utc(1472057321)",
    ]);
    assert_eq!(v[0]["result"], "2016-08-10 12:36:45 UTC");
    assert_eq!(v[1]["result"], "2016-08-10 21:36:45 UTC");
    assert_eq!(v[2]["result"], "2016-08-24 16:49:24 UTC");
    assert_eq!(v[3]["result"], "2016-08-24 16:48:41 UTC");
}

#[test]
fn epoch_boundaries() {
    let v = batch(&[
        "epoch_s_to_utc(0)",
        "epoch_s_to_utc(-1)",
        "epoch_ms_to_utc(1472057364000)",
        "epoch_ns_to_utc(-1)",
    ]);
    assert_eq!(v[0]["result"], "1970-01-01 00:00:00 UTC");
    assert_eq!(v[1]["result"], "1969-12-31 23:59:59 UTC");
    assert_eq!(v[2]["result"], "2016-08-24 16:49:24 UTC");
    assert_eq!(v[3]["result"], "1969-12-31 23:59:59 UTC");
}

#[test]
fn utc_to_epoch_roundtrip() {
    let v = batch(&[
        "utc_to_epoch('2016-08-24 16:49:24 UTC')",
        "utc_to_epoch('2016-08-24T16:49:24Z')",
        "utc_to_epoch('2016-08-24T16:49:24+09:00')",
        "utc_to_epoch('2016-08-24')",
        "utc_to_epoch('2016-08-24 16:49:24')",
    ]);
    assert_eq!(v[0]["result"].as_i64(), Some(1472057364));
    assert_eq!(v[1]["result"].as_i64(), Some(1472057364));
    // 16:49:24+09:00 == 07:49:24 UTC == 1472057364 - 9*3600.
    assert_eq!(v[2]["result"].as_i64(), Some(1472024964));
    // Date-only = midnight UTC.
    assert_eq!(v[3]["result"].as_i64(), Some(1471996800));
    assert_eq!(v[4]["result"].as_i64(), Some(1472057364));
}

#[test]
fn duration_between() {
    // Cerber checkin (16:49:24) minus osk.exe (16:48:41) = 43 s
    // (spec appendix A values).
    let v = batch(&[
        "duration_between('2016-08-24 16:48:41 UTC', '2016-08-24 16:49:24 UTC')",
        "duration_between('2016-08-24 16:49:24 UTC', '2016-08-24 16:48:41 UTC')",
        "duration_between('2016-08-24 16:49:24.5 UTC', '2016-08-24 16:49:25.25 UTC')",
    ]);
    assert_eq!(v[0]["result"].as_i64(), Some(43));
    assert_eq!(v[0]["unit"], "s");
    assert_eq!(v[1]["result"].as_i64(), Some(-43));
    let frac = v[2]["result"].as_f64().unwrap();
    assert!((frac - 0.75).abs() < 1e-12);
}

#[test]
fn tz_convert_fixed_offsets() {
    let v = batch(&[
        "tz_convert('2016-08-24 16:49:24 UTC', '+09:00')",
        "tz_convert('2016-08-24 16:49:24 UTC', 'UTC')",
        "tz_convert('2016-08-24T16:49:24Z', '-08:00')",
    ]);
    assert_eq!(v[0]["result"], "2016-08-25 01:49:24+09:00");
    assert_eq!(v[0]["unit"], "+09:00");
    assert_eq!(v[1]["result"], "2016-08-24 16:49:24 UTC");
    assert_eq!(v[1]["unit"], "UTC");
    assert_eq!(v[2]["result"], "2016-08-24 08:49:24-08:00");

    let v = batch(&["tz_convert('2016-08-24 16:49:24 UTC', 'Asia/Tokyo')"]);
    assert_eq!(err_code(&v, 0), CODE_INVALID_ARGUMENT);
}

// ---------------------------------------------------------------------------
// Arithmetic, units, aggregates
// ---------------------------------------------------------------------------

#[test]
fn arithmetic_edge_cases() {
    let v = batch(&[
        "-5 + 3",
        "1.5 * 2",
        "5 / 2",
        "10 % 3",
        "2 * (3 + 4)",
        "round(2.567)",
        "sum(1, 2, 3)",
        "avg(1, 2, 3)",
        "rate(120, 60)",
    ]);
    assert_eq!(v[0]["result"].as_i64(), Some(-2));
    assert_eq!(v[1]["result"].as_i64(), Some(3));
    assert_eq!(v[2]["result"].as_f64(), Some(2.5));
    assert_eq!(v[3]["result"].as_i64(), Some(1));
    assert_eq!(v[4]["result"].as_i64(), Some(14));
    assert_eq!(v[5]["result"].as_i64(), Some(3));
    assert_eq!(v[6]["result"].as_i64(), Some(6));
    assert_eq!(v[7]["result"].as_i64(), Some(2));
    assert_eq!(v[8]["result"].as_i64(), Some(2));
}

#[test]
fn bytes_units() {
    let v = batch(&[
        "bytes_to_human(1024)",
        "bytes_unit(71176704, 'MiB')",
        "bytes_unit(71176704, 'GB')",
    ]);
    assert_eq!(v[0]["result"], "1 KiB");
    assert_eq!(v[0]["unit"], "KiB");
    let mib = v[1]["result"].as_f64().unwrap();
    assert!((mib - 71176704.0 / 1048576.0).abs() < 1e-12);
    assert_eq!(v[1]["unit"], "MiB");
    let gb = v[2]["result"].as_f64().unwrap();
    assert!((gb - 71176704.0 / 1073741824.0).abs() < 1e-12);
}

// ---------------------------------------------------------------------------
// Decode / normalize / json_get
// ---------------------------------------------------------------------------

#[test]
fn decode_roundtrips() {
    let v = batch(&[
        "base64_encode('hello')",
        "base64_decode('aGVsbG8=')",
        "base64_decode('aGVs\\nbG8=')",
        "base64_decode('aGVsbG8')", // unpadded
        "hex_encode('ABC')",
        "hex_decode('414243')",
        "url_encode('a b&c=1')",
        "url_decode('a%20b%26c%3D1')",
        "url_decode('a+b')",
    ]);
    assert_eq!(v[0]["result"], "aGVsbG8=");
    assert_eq!(v[1]["result"], "hello");
    assert_eq!(v[2]["result"], "hello");
    assert_eq!(v[3]["result"], "hello");
    assert_eq!(v[4]["result"], "414243");
    assert_eq!(v[5]["result"], "ABC");
    assert_eq!(v[6]["result"], "a%20b%26c%3D1");
    assert_eq!(v[7]["result"], "a b&c=1");
    assert_eq!(v[8]["result"], "a b");
}

#[test]
fn json_get_and_normalize() {
    let v = batch(&[
        "json_get('{\"a\":{\"b\":[1,2,3]}}', 'a.b[1]')",
        "json_get('{\"a\":5}', '$.a')",
        "json_get('not json', 'a')",
        "json_get('{\"a\":{\"b\":1}}', 'a.c')",
        "normalize('  MiXeD   Case-String  ')",
    ]);
    assert_eq!(v[0]["result"].as_i64(), Some(2));
    assert_eq!(v[1]["result"].as_i64(), Some(5));
    assert_eq!(err_code(&v, 2), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 3), CODE_INVALID_ARGUMENT);
    assert_eq!(v[4]["result"], "mixed case-string");
}

// ---------------------------------------------------------------------------
// Error codes (spec: error codes) and partial failure isolation
// ---------------------------------------------------------------------------

#[test]
fn error_codes() {
    let v = batch(&[
        "1 +",
        "foo(1)",
        "epoch_s_to_utc('abc')",
        "1 / 0",
        "1e308 * 10",
        "round(2.5, -1)",
        "bytes_unit(1, 'XYZ')",
        "hex_decode('abc')",
    ]);
    assert_eq!(err_code(&v, 0), CODE_PARSE_ERROR);
    assert_eq!(err_code(&v, 1), CODE_UNKNOWN_FUNCTION);
    assert_eq!(err_code(&v, 2), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 3), CODE_VALUE_OUT_OF_RANGE);
    assert_eq!(err_code(&v, 4), CODE_VALUE_OUT_OF_RANGE);
    assert_eq!(err_code(&v, 5), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 6), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 7), CODE_INVALID_ARGUMENT);

    // Spec partial-failure example: one failure never affects the others.
    let v = batch(&["epoch_s_to_utc('abc')", "1425 * 32"]);
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(err_code(&v, 0), CODE_INVALID_ARGUMENT);
    assert_eq!(v[1]["result"].as_i64(), Some(45600));
}

#[test]
fn whole_call_errors() {
    // Missing / wrong-typed / empty expressions.
    let v = execute_calc(&json!({}), None);
    assert_eq!(v["error"]["code"], CODE_INVALID_ARGUMENT);
    let v = execute_calc(&json!({ "expressions": "1 + 2" }), None);
    assert_eq!(v["error"]["code"], CODE_INVALID_ARGUMENT);
    let v = execute_calc(&json!({ "expressions": [] }), None);
    assert_eq!(v["error"]["code"], CODE_INVALID_ARGUMENT);

    // Batch limit: 51 expressions -> LIMIT_EXCEEDED for the whole call.
    let many: Vec<String> = (0..51).map(|i| format!("{}", i)).collect();
    let v = execute_calc(&json!({ "expressions": many }), None);
    assert_eq!(v["error"]["code"], CODE_LIMIT_EXCEEDED);

    // Per-expression length limit: 501 chars -> per-element error only.
    let long = "x".repeat(501).to_string();
    let v = batch(&[&long, "1 + 1"]);
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(err_code(&v, 0), CODE_LIMIT_EXCEEDED);
    assert_eq!(v[1]["result"].as_i64(), Some(2));
}

// ---------------------------------------------------------------------------
// Malicious / adversarial input robustness
// ---------------------------------------------------------------------------

#[test]
fn nonfinite_results_are_rejected() {
    // percent/rate must not leak inf/NaN into the result (val_to_json would
    // silently turn them into JSON null).
    let v = batch(&[
        "percent(1e308, 1)",
        "percent(1e308, 1e-300)",
        "rate(1e308, 1e-300)",
    ]);
    for i in 0..3 {
        assert_eq!(err_code(&v, i), CODE_VALUE_OUT_OF_RANGE, "idx {}", i);
    }
}

#[test]
fn overflow_and_range_edges() {
    let v = batch(&[
        "9223372036854775807 + 1",             // i64 checked_add overflow
        "9223372036854775807 * 2",             // i64 checked_mul overflow
        "epoch_s_to_utc(9223372036854775807)", // chrono out of range
        "1.5 / 0",                             // float division by zero
        "0.0 / 0.0",                           // NaN path -> division by zero
    ]);
    for i in 0..5 {
        assert_eq!(err_code(&v, i), CODE_VALUE_OUT_OF_RANGE, "idx {}", i);
    }
}

#[test]
fn parser_rejects_garbage() {
    let v = batch(&[
        "1 2",
        "'unterminated",
        "()",
        "foo",
        "1 +",
        "0x10",
        "1.2.3",
        "1e999",
        "",
    ]);
    for i in 0..v.as_array().unwrap().len() {
        assert_eq!(err_code(&v, i), CODE_PARSE_ERROR, "expr idx {}", i);
    }
}

#[test]
fn max_depth_nesting_and_exact_limits() {
    // At the nesting cap: 99 unary minuses still parse and evaluate.
    let negs = format!("{}1", "-".repeat(MAX_EXPR_NESTING - 1));
    let v = batch(&[&negs]);
    assert_eq!(v[0]["result"].as_i64(), Some(-1));

    // 99 nested parens: still at the cap.
    let nested = format!(
        "{}1{}",
        "(".repeat(MAX_EXPR_NESTING - 1),
        ")".repeat(MAX_EXPR_NESTING - 1)
    );
    let v = batch(&[&nested]);
    assert_eq!(v[0]["result"].as_i64(), Some(1));

    // Beyond the cap: rejected with LIMIT_EXCEEDED, independently of the
    // character limit.
    let parens_deep = format!(
        "{}1{}",
        "(".repeat(MAX_EXPR_NESTING + 1),
        ")".repeat(MAX_EXPR_NESTING + 1)
    );
    let negs_deep = format!("{}1", "-".repeat(MAX_EXPR_NESTING + 1));
    let v = batch(&[&parens_deep, &negs_deep]);
    for i in 0..2 {
        assert_eq!(err_code(&v, i), CODE_LIMIT_EXCEEDED, "idx {}", i);
    }

    // Exactly 50 expressions is allowed; every element succeeds.
    let exprs: Vec<String> = (0..50).map(|i| format!("{} + 0", i)).collect();
    let refs: Vec<&str> = exprs.iter().map(|s| s.as_str()).collect();
    let v = batch(&refs);
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 50);
    assert_eq!(arr[0]["result"].as_i64(), Some(0));
    assert_eq!(arr[49]["result"].as_i64(), Some(49));
    for el in arr {
        assert!(el.get("error").is_none(), "no element may fail: {}", el);
    }
}

#[test]
fn json_path_and_decode_edges() {
    let v = batch(&[
        "json_get('{\"a\":[1,2]}', 'a[5]')",  // index out of range
        "json_get('{\"a\":[1,2]}', 'a[-1]')", // negative index: invalid path
        "json_get('{\"a b\":1}', 'a b')",     // key containing a space
        "base64_decode('aHR0cHM6Ly8')",       // unpadded
        "base64_decode('***')",               // invalid alphabet
        "hex_decode('gg')",                   // invalid hex char
    ]);
    assert_eq!(err_code(&v, 0), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 1), CODE_INVALID_ARGUMENT);
    assert_eq!(v[2]["result"].as_i64(), Some(1));
    assert_eq!(v[3]["result"], "https://");
    assert_eq!(err_code(&v, 4), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 5), CODE_INVALID_ARGUMENT);
}

#[test]
fn tz_and_byte_unit_edges() {
    let v = batch(&[
        "tz_convert('2016-08-24 16:49:24 UTC', '+9')",
        "tz_convert('2016-08-24 16:49:24 UTC', '+0900')",
        "tz_convert('2016-08-24 16:49:24 UTC', '+24:00')",
        "bytes_unit(2048, 'kb')",
        "bytes_unit(1048576, 'MB')",
        "bytes_to_human(0)",
        "bytes_to_human(1e300)",
    ]);
    assert_eq!(v[0]["result"], "2016-08-25 01:49:24+09:00");
    assert_eq!(v[1]["result"], "2016-08-25 01:49:24+09:00");
    assert_eq!(err_code(&v, 2), CODE_INVALID_ARGUMENT);
    assert_eq!(v[3]["result"].as_i64(), Some(2));
    assert_eq!(v[3]["unit"], "KiB");
    assert_eq!(v[4]["result"].as_i64(), Some(1));
    assert_eq!(v[4]["unit"], "MiB");
    assert_eq!(v[5]["result"], "0 B");
    // Huge value: no i64 saturation, no crash, unit still attached.
    let huge = v[6]["result"].as_str().unwrap();
    assert!(huge.ends_with("PiB"), "{}", huge);
}

#[test]
fn tz_convert_non_ascii_zone_no_panic() {
    // Multi-byte offsets must fail cleanly, never panic on a non-UTF-8
    // boundary byte slice (regression for the split_at byte-index panic).
    let v = batch(&[
        "tz_convert('2023-01-01 00:00:00 UTC', '+🚀')",
        "tz_convert('2023-01-01 00:00:00 UTC', '+あ')",
        "tz_convert('2023-01-01 00:00:00 UTC', '+é9')",
        "tz_convert('2023-01-01 00:00:00 UTC', '+お:00')",
    ]);
    for i in 0..v.as_array().unwrap().len() {
        assert_eq!(err_code(&v, i), CODE_INVALID_ARGUMENT, "idx {}", i);
    }
}

#[test]
fn ledger_path_label_sanitized() {
    let ws = Path::new("/ws");
    let data = Path::new("/data");
    // Traversal attempts must stay inside the data dir.
    let p = resolve_ledger_path("../../../etc/cron.d/malicious", 0, ws, Some(data)).unwrap();
    assert!(p.starts_with(data), "{}", p.display());
    let name = p.file_name().unwrap().to_str().unwrap();
    assert!(!name.contains('/') && !name.contains(".."), "{}", name);

    // Empty label falls back to a fixed name.
    let p = resolve_ledger_path("", 0, ws, Some(data)).unwrap();
    assert_eq!(p.file_name().unwrap(), "calc_ledger_unnamed.jsonl");

    // Overlong labels are capped so the path stays short.
    let long = "x".repeat(200);
    let p = resolve_ledger_path(&long, 0, ws, Some(data)).unwrap();
    assert!(p.file_name().unwrap().to_str().unwrap().len() < 100);
}

#[test]
fn json_path_rejects_special_chars() {
    // Keys absorb only alnum/_/-/space; anything else terminates the key
    // and is rejected instead of being silently absorbed.
    let v = batch(&[
        "json_get('{\"a/b\":1}', 'a/b')",
        "json_get('{\"a\":1}', 'a$')",
    ]);
    assert_eq!(err_code(&v, 0), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 1), CODE_INVALID_ARGUMENT);
}

#[test]
fn integer_literal_out_of_range() {
    // Integer literals beyond i64 must error, not silently round via f64
    // (spec promise: integer literals are kept exact).
    let v = batch(&[
        "99999999999999999999999 + 1",
        "epoch_ns_to_utc(99999999999999999999)",
    ]);
    assert_eq!(err_code(&v, 0), CODE_VALUE_OUT_OF_RANGE);
    assert_eq!(err_code(&v, 1), CODE_VALUE_OUT_OF_RANGE);
}

#[test]
fn arity_error_branches() {
    // Arity guards: too few / too many arguments fail with
    // INVALID_ARGUMENT; exact and minimum counts succeed.
    let v = batch(&[
        "percent(1)",           // 1 of 2 required
        "sum()",                // 0 of min 1
        "round(1, 2, 3)",       // 3 of max 2
        "epoch_s_to_utc(1, 2)", // 2 of exactly 1
        "percent(1, 2)",        // exact: ok
        "round(2.5)",           // minimum: ok (digits optional)
    ]);
    assert_eq!(err_code(&v, 0), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 1), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 2), CODE_INVALID_ARGUMENT);
    assert_eq!(err_code(&v, 3), CODE_INVALID_ARGUMENT);
    assert_eq!(v[4]["result"].as_i64(), Some(50));
    assert_eq!(v[5]["result"].as_i64(), Some(3));
}

#[test]
fn operator_chain_depth_is_bounded() {
    // Left-deep binary chains are bounded by MAX_EXPR_NESTING (not by the
    // character limit): raising MAX_EXPRESSION_CHARS must never grow the
    // eval recursion.
    let at_cap = "1+".repeat(MAX_EXPR_NESTING - 1) + "1"; // 100 terms, depth 100: allowed
    let v = batch(&[&at_cap]);
    assert_eq!(v[0]["result"].as_i64(), Some(100));

    // 151 terms: depth 151, well below the 500-char limit (302 chars)
    // but above the nesting cap: rejected.
    let over = "1+".repeat(MAX_EXPR_NESTING + 50) + "1";
    assert!(over.chars().count() < MAX_EXPRESSION_CHARS);
    let v = batch(&[&over]);
    assert_eq!(err_code(&v, 0), CODE_LIMIT_EXCEEDED);
}

// ---------------------------------------------------------------------------
// calc_id numbering / result shape
// ---------------------------------------------------------------------------

#[test]
fn calc_id_format_and_batch_order() {
    let v = batch(&["1 + 1", "2 + 2", "bad(", "3 + 3"]);
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 4);
    let id_re = regex::Regex::new(r"^C-\d{4}$").unwrap();
    let mut prev = String::new();
    for el in arr {
        let id = el["calc_id"].as_str().unwrap();
        assert!(id_re.is_match(id), "calc_id format: {}", id);
        assert!(id > prev.as_str(), "calc_ids must be strictly increasing");
        prev = id.to_string();
    }
    assert_eq!(arr[2]["error"]["code"], CODE_PARSE_ERROR);
}

// ---------------------------------------------------------------------------
// Number ledger (spec: number ledger)
// ---------------------------------------------------------------------------

#[test]
fn ledger_path_resolution() {
    let ws = Path::new("/ws");
    let data = Path::new("/data");
    let p = resolve_ledger_path("mylabel", 0, ws, Some(data)).unwrap();
    assert_eq!(p, Path::new("/data/calc_ledger_mylabel.jsonl"));
    let p = resolve_ledger_path("mylabel", 1, ws, Some(data)).unwrap();
    assert_eq!(p, Path::new("/ws/artifacts/calc_ledger.jsonl"));
    assert!(resolve_ledger_path("mylabel", 0, ws, None).is_none());
}

#[test]
fn ledger_records_success_and_error() {
    let dir = std::env::temp_dir().join(format!(
        "agt_calc_ledger_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("calc_ledger.jsonl");

    let ledger = CalcLedger::at(path.clone());
    execute_calc(
        &json!({ "expressions": ["percent(17121, 17267)", "1 / 0"] }),
        Some(&ledger),
    );

    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one record per expression: {}", text);

    let ok_rec: Value = serde_json::from_str(lines[0]).unwrap();
    assert!(ok_rec["calc_id"].as_str().unwrap().starts_with("C-"));
    assert_eq!(ok_rec["expression"], "percent(17121, 17267)");
    assert_eq!(ok_rec["inputs"]["part"].as_i64(), Some(17121));
    assert_eq!(ok_rec["inputs"]["whole"].as_i64(), Some(17267));
    assert!(ok_rec["result"].as_f64().is_some());
    assert_eq!(ok_rec["source"], "calc");
    assert!(ok_rec["recorded_at"].as_str().is_some());

    let err_rec: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(err_rec["expression"], "1 / 0");
    assert_eq!(err_rec["error"]["code"], CODE_VALUE_OUT_OF_RANGE);
    assert!(err_rec.get("inputs").is_none());
    assert!(err_rec.get("result").is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Integration with the tool definitions and dispatcher
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_definition_and_dispatch() {
    let defs = crate::tools::get_tool_definitions(None, |_| true);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"calc"), "calc must be in default tool list");

    let defs = crate::tools::get_tool_definitions(None, |n| n != "calc");
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"calc"),
        "calc must be hidden when disabled"
    );

    // Dispatch through execute_tool, no ledger attached.
    let res = crate::tools::execute_tool(
        "calc",
        &json!({ "expressions": ["1425 * 32"] }),
        None,
        None,
        0,
        |_| true,
    )
    .await
    .unwrap();
    assert_eq!(res[0]["result"].as_i64(), Some(45600));

    // Disabled tools are refused even if called (defense in depth).
    let res = crate::tools::execute_tool(
        "calc",
        &json!({ "expressions": ["1 + 1"] }),
        None,
        None,
        0,
        |_| false,
    )
    .await;
    let err = res.unwrap_err().to_string();
    assert!(err.contains("[TOOL_DISABLED]"), "{}", err);
}
