#![allow(clippy::many_single_char_names)]

use serde_json::{Value, json};

use super::*;

fn layer(t: f64) -> Frame<'static> {
    let env = Env {
        fps: 30.0,
        canvas: [1080.0, 1920.0],
        size: [1080.0, 632.0],
        source: [3840.0, 2160.0],
        in_: 1.0,
        out: 5.0,
        total: 6.0,
        source_in: 10.0,
        vars: None,
    };
    #[allow(clippy::cast_possible_truncation)]
    let mut b = env.builtins((t * 30.0).round() as i64);
    b[Builtin::T as usize] = t;
    b[Builtin::Lt as usize] = t - 1.0;
    Frame { b, vars: &[] }
}

fn ev(src: &str, f: &Frame) -> f64 {
    Compiled::new(src, &Scope::new(Clock::Layer))
        .unwrap_or_else(|e| panic!("{src}: {e:?}"))
        .eval(f)
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn eases_match_golden_tables() {
    let us = [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];
    let table: [(Ease, [f64; 7]); 7] = [
        (Ease::Linear, us),
        (
            Ease::Ease,
            [
                0.0,
                0.024_471_742,
                0.146_446_609,
                0.5,
                0.853_553_391,
                0.975_528_258,
                1.0,
            ],
        ),
        (
            Ease::In,
            [0.0, 0.001, 0.015_625, 0.125, 0.421_875, 0.729, 1.0],
        ),
        (
            Ease::Out,
            [0.0, 0.271, 0.578_125, 0.875, 0.984_375, 0.999, 1.0],
        ),
        (Ease::InOut, [0.0, 0.004, 0.0625, 0.5, 0.9375, 0.996, 1.0]),
        (
            Ease::Back,
            [
                0.0,
                0.408_827_98,
                0.817_409_688,
                1.087_697_5,
                1.064_136_563,
                1.014_314_22,
                1.0,
            ],
        ),
        (
            Ease::Smoothstep,
            [0.0, 0.028, 0.156_25, 0.5, 0.843_75, 0.972, 1.0],
        ),
    ];
    for (e, want) in table {
        for (u, w) in us.iter().zip(want) {
            assert!(close(e.at(*u), w, 1e-9), "{e:?}({u}) = {} != {w}", e.at(*u));
        }
        // Clamped outside 0..1.
        assert!(close(e.at(-1.0), want[0], 1e-12) && close(e.at(2.0), 1.0, 1e-12));
    }
    assert!(close(Ease::Hold.at(0.99), 0.0, 0.0) && close(Ease::Hold.at(1.0), 1.0, 0.0));
}

#[test]
fn bezier_matches_css() {
    // CSS's named timing functions, y at x = 0.1, 0.25, 0.5, 0.75, 0.9.
    let xs = [0.1, 0.25, 0.5, 0.75, 0.9];
    let css: [([f64; 4], [f64; 5]); 4] = [
        // ease
        (
            [0.25, 0.1, 0.25, 1.0],
            [0.094_796, 0.408_511, 0.802_403, 0.960_459, 0.994_316],
        ),
        // ease-in
        (
            [0.42, 0.0, 1.0, 1.0],
            [0.017_027, 0.093_465, 0.315_357, 0.621_862, 0.839_428],
        ),
        // ease-out
        (
            [0.0, 0.0, 0.58, 1.0],
            [0.160_572, 0.378_138, 0.684_643, 0.906_535, 0.982_973],
        ),
        // ease-in-out
        (
            [0.42, 0.0, 0.58, 1.0],
            [0.019_722, 0.129_162, 0.5, 0.870_838, 0.980_278],
        ),
    ];
    for (c, want) in css {
        for (x, w) in xs.iter().zip(want) {
            let y = Ease::Bezier(c).at(*x);
            assert!(close(y, w, 2e-6), "cubic-bezier{c:?}({x}) = {y} != {w}");
        }
    }
    let f = layer(0.0);
    assert!(close(
        ev("bezier(0.25, 0.1, 0.25, 1, 0.5)", &f),
        0.802_403,
        2e-6
    ));
}

#[test]
fn matches_av_expr_eval() {
    let doc: Value =
        serde_json::from_str(include_str!("testdata/ffmpeg_eval.json")).expect("fixture");
    let t = doc["vars"]["t"].as_f64().expect("t");
    let f = layer(t);
    let rows = doc["rows"].as_array().expect("rows");
    assert!(rows.len() > 90);
    for r in rows {
        let (ours, want) = (r[0].as_str().expect("expr"), r[2].as_f64().expect("value"));
        let got = ev(ours, &f);
        assert!(
            close(got, want, 1e-12 * want.abs().max(1.0)),
            "{ours} = {got}, ffmpeg's {} = {want}",
            r[1]
        );
    }
}

#[test]
fn grammar_precedence_and_associativity() {
    let f = layer(2.0);
    for (src, want) in [
        ("1 + 2 * 3", 7.0),
        ("10 - 4 - 3", 3.0),
        ("64 / 4 / 2", 8.0),
        ("7 - 5 % 3", 5.0),
        // `^` binds tighter than unary minus on its left and is right-associative.
        ("-2^2", -4.0),
        ("2^3^2", 512.0),
        ("2^-1", 0.5),
        ("(2^3)^2", 64.0),
        ("-3 * -2", 6.0),
        ("!0 + 1", 2.0),
        ("!(0 + 1)", 0.0),
        // Comparisons below arithmetic, `&&` below comparisons, `||` below `&&`.
        ("1 + 1 == 2", 1.0),
        ("0 || 1 && 0", 0.0),
        ("1 || 0 && 0", 1.0),
        // `?:` is lowest and nests to the right.
        ("0 ? 1 : 0 ? 2 : 3", 3.0),
        ("1 ? 0 ? 5 : 6 : 7", 6.0),
        ("1 + 1 ? 4 : 5", 4.0),
        // Variables next to functions of the same name.
        ("lt(lt, 2)", 1.0),
        ("t + lt + st + n", 2.0 + 1.0 + 11.0 + 60.0),
        (
            "W + H + w + h + sw + sh",
            1080.0 + 1920.0 + 1080.0 + 632.0 + 3840.0 + 2160.0,
        ),
        ("in + out + dur + T + fps", 1.0 + 5.0 + 4.0 + 6.0 + 30.0),
        ("min(4, 2, 8, 3) + max(1, 9, 2)", 11.0),
        ("smoothstep(0, 4, 1)", 0.156_25),
        ("ease_out(0.5) + ease_in(0.5)", 1.0),
        (
            "ease(0.5) + ease_in_out(0.25) + back(1)",
            0.5 + 0.0625 + 1.0,
        ),
        (".5 + 1.", 1.5),
    ] {
        assert!(
            close(ev(src, &f), want, 1e-12),
            "{src} = {} != {want}",
            ev(src, &f)
        );
    }
}

fn compile_err(src: &str) -> ExprError {
    Compiled::new(src, &Scope::new(Clock::Layer)).expect_err(src)
}

#[test]
fn errors_report_columns() {
    for (src, col, msg) in [
        ("if(lt(t,4.7), 0", 16, "expected \")\""),
        ("1 +", 4, "unexpected end"),
        ("2 * (3", 7, "expected \")\""),
        ("foo + 1", 1, "unknown variable `foo`"),
        ("1 + bar(2)", 5, "unknown function `bar`"),
        ("clip(1, 2)", 1, "clip() takes 3 arguments, not 2"),
        ("min(1)", 1, "min() takes 2 or more arguments, not 1"),
        ("if(1)", 1, "if() takes 2 or 3 arguments, not 1"),
        ("1 < 2 < 3", 7, "comparisons don't chain"),
        ("t = 1", 3, "unexpected `=`"),
        ("1 $ 2", 3, "unexpected `$`"),
        ("1e+", 4, "expected exponent digits"),
        ("value * 2", 1, "`value` is only defined"),
        ("(1) 2", 5, "expected an operator or the end"),
    ] {
        let e = compile_err(src);
        assert_eq!(e.col, col, "{src}: {e:?}");
        assert!(e.msg.starts_with(msg), "{src}: {e:?}");
    }
    let deep = format!("{}1{}", "(".repeat(70), ")".repeat(70));
    assert!(compile_err(&deep).msg.contains("deeper than 64"));
    let long = "1+".repeat(2100) + "1";
    assert!(compile_err(&long).msg.contains("longer than 4096"));

    let e = Track::<f64>::parse(&json!("if(lt(t,4.7), 0"), Clock::Layer, "layers[2].fade")
        .expect_err("syntax");
    assert_eq!(
        e.to_string(),
        "layers[2].fade: col 16: expected \")\"\n  if(lt(t,4.7), 0\n                 ^"
    );
    let e = Track::<[f64; 2]>::parse(&json!([0, "1 +* 2"]), Clock::Layer, "p").expect_err("x");
    assert!(
        e.to_string().starts_with("p[1]: col 4: unexpected \"*\""),
        "{e}"
    );
}

#[test]
fn clocks_limit_variables() {
    assert!(Compiled::new("st * 2", &Scope::new(Clock::Source)).is_ok());
    let e = Compiled::new("t * 2", &Scope::new(Clock::Source)).expect_err("t");
    assert!(e.msg.contains("isn't known"), "{e:?}");
    assert!(Compiled::new("lt / dur", &Scope::new(Clock::Clip)).is_ok());
    let e = Compiled::new("random(1)", &Scope::new(Clock::Source)).expect_err("n");
    assert!(e.msg.contains("frame index"), "{e:?}");
}

#[test]
fn random_and_noise_are_stable() {
    // Fixed values: the same on every run, platform and piece.
    let r: Vec<f64> = (0..4).map(|n| expr::random(7.0, f64::from(n))).collect();
    let want = RANDOM_7;
    for (a, b) in r.iter().zip(want) {
        assert!(close(*a, b, 1e-15), "{r:?}");
    }
    assert!(r.iter().all(|v| (0.0..1.0).contains(v)));
    assert!(close(expr::random(-0.0, 3.0), expr::random(0.0, 3.0), 0.0));
    let nz: Vec<f64> = [0.0, 0.5, 1.25, 7.9]
        .iter()
        .map(|x| expr::noise(3.0, *x))
        .collect();
    for (a, b) in nz.iter().zip(NOISE_3) {
        assert!(close(*a, b, 1e-15), "{nz:?}");
    }
    // Smooth: small steps change it little, and it stays in [-1, 1].
    for i in 0..1000 {
        let x = f64::from(i) * 0.013;
        let (a, b) = (expr::noise(1.0, x), expr::noise(1.0, x + 1e-4));
        assert!((-1.0..=1.0).contains(&a) && (a - b).abs() < 1e-3);
    }
    // `random` reads the frame index.
    let (f0, f1) = (layer(0.0), layer(1.0));
    assert!(close(ev("random(7)", &f0), want[0], 0.0));
    assert!(close(ev("random(7)", &f1), expr::random(7.0, 30.0), 0.0));
}

const RANDOM_7: [f64; 4] = [
    0.024_145_070_830_332_793,
    0.437_948_665_686_379_47,
    0.448_670_800_477_816_05,
    0.515_853_693_302_742,
];
const NOISE_3: [f64; 4] = [
    0.131_544_359_185_369_77,
    0.543_012_620_893_750_9,
    0.954_024_229_210_387_4,
    -0.392_725_469_971_708_3,
];

#[test]
fn constant_folding() {
    let s = Scope::new(Clock::Layer);
    assert_eq!(
        Compiled::new("2 * PI + if(1, 3)", &s)
            .expect("c")
            .as_const(),
        Some(2.0 * PI + 3.0)
    );
    assert_eq!(Compiled::new("t * 2", &s).expect("c").as_const(), None);
    // An all-constant track becomes `Const`.
    assert_eq!(
        Track::<[f64; 2]>::parse(&json!(["1 + 1", 3]), Clock::Layer, "p").expect("p"),
        Track::Const([2.0, 3.0])
    );
}

#[test]
fn tracks_parse_every_form() {
    let p = |v: Value| Track::<f64>::parse(&v, Clock::Layer, "x").expect("parse");
    assert_eq!(p(json!(0.5)), Track::Const(0.5));
    let k = p(json!([{"t": 1, "v": 2}, {"t": 0, "v": 0, "ease": "out"}]));
    assert!(k.is_animated());
    // At lt = 0.5: out(0.5) * 2.
    let mut f = layer(1.5);
    f.b[Builtin::Lt as usize] = 0.5;
    assert!(close(
        k.eval(&f).expect("v"),
        Ease::Out.at(0.5) * 2.0,
        1e-12
    ));

    let e = p(json!("lerp(1, 1.06, lt / dur)"));
    assert!(close(e.eval(&layer(3.0)).expect("v"), 1.03, 1e-12));

    let ke = p(json!({"keys": [{"t": 0, "v": 0}, {"t": 0.4, "v": 1}], "expr": "value * 10 + t"}));
    let mut f = layer(1.2);
    f.b[Builtin::Lt as usize] = 0.2;
    assert!(close(ke.eval(&f).expect("v"), 5.0 + 1.2, 1e-12));

    let v = Track::<[f64; 2]>::parse(&json!(["H*0.5 + 20*sin(t)", 644]), Clock::Layer, "pos")
        .expect("vec");
    let at = v.eval(&layer(0.0)).expect("v");
    assert!(close(at[0], 960.0, 1e-9) && close(at[1], 644.0, 0.0));

    let c = Track::<[f64; 4]>::parse(&json!("#ff8000"), Clock::Layer, "c").expect("colour");
    assert_eq!(c, Track::Const([1.0, 128.0 / 255.0, 0.0, 1.0]));
    assert_eq!(
        Track::<[f64; 3]>::parse(&json!("black"), Clock::Layer, "c").expect("black"),
        Track::Const([0.0; 3])
    );
    // A colour string is never an expression.
    assert!(Track::<[f64; 3]>::parse(&json!("blue"), Clock::Layer, "c").is_err());
    assert!(Track::<f64>::parse(&json!(true), Clock::Layer, "x").is_err());
    assert!(
        Track::<f64>::parse(
            &json!([{"t": 0, "v": 1, "interp": "bounce"}]),
            Clock::Layer,
            "x"
        )
        .is_err()
    );
}

#[test]
fn non_finite_is_an_error() {
    let t = Track::<f64>::parse(&json!("log(t - 3)"), Clock::Layer, "x").expect("p");
    let e = t.eval(&layer(1.0)).expect_err("nan");
    assert!(e.to_string().contains("not finite"), "{e}");
    // An untaken branch doesn't count.
    let t =
        Track::<f64>::parse(&json!("if(gt(t, 3), log(t - 3), 0)"), Clock::Layer, "x").expect("p");
    assert!(close(t.eval(&layer(1.0)).expect("v"), 0.0, 0.0));
}

#[test]
fn keys_on_each_clock() {
    let keys = json!([{"t": 0, "v": 0}, {"t": -1, "v": 1}]);
    let k = Track::<f64>::parse(&keys, Clock::Clip, "x").expect("p");
    // Negative times count from the end of a 5 s clip: the second key is at 4 s.
    assert!(close(k.at(Clock::Clip, 2.0, 5.0).expect("v"), 0.5, 1e-12));
    let s = Track::<f64>::parse(
        &json!([{"t": 10, "v": 0}, {"t": 12, "v": 4}]),
        Clock::Source,
        "x",
    )
    .expect("p");
    assert!(close(
        s.at(Clock::Source, 11.0, 0.0).expect("v"),
        2.0,
        1e-12
    ));
    let e = Track::<f64>::parse(&json!("st * 2"), Clock::Source, "x").expect("p");
    assert!(close(e.at(Clock::Source, 11.0, 0.0).expect("v"), 22.0, 0.0));
}

#[test]
fn sample_and_vars() {
    let mut vars = Vars::new();
    vars.push_level(
        json!({"enter": 0.25, "swap": 2.5, "swap_step": 0.12})
            .as_object()
            .expect("o"),
        Clock::Layer,
        "vars",
    )
    .expect("comp vars");
    vars.push_level(
        json!({"s": "swap + 2 * swap_step", "kick": [{"t": 2, "v": 0, "interp": "out"}, {"t": 2.2, "v": 1}],
               "late": "s + 1"})
        .as_object()
        .expect("o"),
        Clock::Layer,
        "comps.tile2.vars",
    )
    .expect("tile vars");
    // An inner level shadows, and the outer vars keep their own meaning.
    vars.push_level(
        json!({"swap": 10}).as_object().expect("o"),
        Clock::Layer,
        "layers[0].vars",
    )
    .expect("layer vars");
    let scope = Scope::with_vars(Clock::Layer, &vars);
    let t = Track::<f64>::parse_in(&json!("late + swap + kick"), &scope, "x").expect("p");
    let env = Env {
        fps: 10.0,
        out: 10.0,
        vars: Some(&vars),
        ..Env::default()
    };
    let v = t.sample(Clock::Layer, 20..23, &env).expect("sample");
    assert!(close(v[0], 2.74 + 1.0 + 10.0, 1e-12), "{v:?}");
    assert!(close(v[1], 3.74 + 10.0 + Ease::Out.at(0.5), 1e-12), "{v:?}");

    let cyc = Vars::new()
        .push_level(
            json!({"a": "b + 1", "b": "c", "c": "a * 2", "d": 1})
                .as_object()
                .expect("o"),
            Clock::Layer,
            "vars",
        )
        .expect_err("cycle");
    assert!(
        cyc.to_string().contains("vars cycle: a -> b -> c -> a"),
        "{cyc}"
    );
    let shadow = Vars::new()
        .push_level(
            json!({"t": 1}).as_object().expect("o"),
            Clock::Layer,
            "vars",
        )
        .expect_err("shadow");
    assert!(shadow.to_string().contains("built-in"), "{shadow}");
    let shadow = Vars::new()
        .push_level(
            json!({"sin": 1}).as_object().expect("o"),
            Clock::Layer,
            "vars",
        )
        .expect_err("shadow");
    assert!(shadow.to_string().contains("built-in"), "{shadow}");

    // `Sampled` keeps exact values from the range's start.
    let s = Track::Sampled(vec![0.1, 0.2, 0.3]);
    assert_eq!(
        s.sample(Clock::Layer, 5..8, &Env::default()).expect("s"),
        [0.1, 0.2, 0.3]
    );
}

#[test]
fn constant_non_finite_is_a_compile_error() {
    for src in ["1/0", "log(-1)", "0/0", "sqrt(-1)", "-1/0 + 2"] {
        let e = compile_err(src);
        assert!(e.msg.contains("not finite") && e.col == 1, "{src}: {e:?}");
        assert!(Track::<f64>::parse(&json!(src), Clock::Layer, "x").is_err());
        let ke = json!({"keys": [{"t": 0, "v": 1}], "expr": src});
        assert!(Track::<f64>::parse(&ke, Clock::Layer, "x").is_err());
    }
    // A non-finite constant in an untaken branch still compiles.
    assert!(Compiled::new("if(t, 1, 1/0)", &Scope::new(Clock::Layer)).is_ok());
}

#[test]
fn long_chains_are_capped_not_a_stack_overflow() {
    let ok = format!("1{}", "+t".repeat(60));
    assert!(close(ev(&ok, &layer(2.0)), 121.0, 1e-9));
    // Just under the 4 KiB limit, so the depth check is what stops them.
    for op in ["+", "*", "&&", "||"] {
        let long = format!("1{}", format!("{op}t").repeat(4090 / (op.len() + 1)));
        let e = compile_err(&long);
        assert!(e.msg.contains("deeper than 64"), "{op}: {e:?}");
    }
    // The top-level expression takes one level, so a chain is at most 63 operators long.
    let at_limit = format!("1{}", "+t".repeat(63));
    assert!(close(ev(&at_limit, &layer(2.0)), 127.0, 1e-9));
    let over = format!("1{}", "+t".repeat(64));
    assert!(
        compile_err(&over).msg.contains("deeper than 64"),
        "64 operators"
    );
}

#[test]
fn calls_take_at_most_255_arguments() {
    let args = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    // 255 arguments: t and 0..=253.
    assert!(close(
        ev(&format!("max(t, {})", args(254)), &layer(0.5)),
        253.0,
        0.0
    ));
    let e = compile_err(&format!("max(t, {})", args(300)));
    assert_eq!(e.col, 1, "{e:?}");
    assert!(e.msg.contains("at most 255"), "{e:?}");
}

#[test]
fn nan_time_and_bezier_bounds() {
    let k = [
        Key {
            t: 0.0,
            v: 1.0,
            ease: Ease::Linear,
        },
        Key {
            t: 1.0,
            v: 2.0,
            ease: Ease::Linear,
        },
    ];
    assert_eq!(keys_at(&k, f64::NAN), Some(1.0));
    assert!(Ease::parse(&json!([0.25, 0.1, 0.25, 1.0])).is_some());
    assert!(Ease::parse(&json!([1.5, 0.1, 0.25, 1.0])).is_none());
    assert!(Ease::parse(&json!([0.25, 0.1, -0.1, 1.0])).is_none());
    // y may leave 0..1 (overshoot).
    assert!(Ease::parse(&json!([0.3, -0.5, 0.7, 1.5])).is_some());
}

#[test]
fn an_inner_var_named_like_an_outer_one_is_a_cycle() {
    let mut v = Vars::new();
    let outer = json!({"x": 2});
    v.push_level(outer.as_object().expect("o"), Clock::Layer, "comp.vars")
        .expect("outer");
    let inner = json!({"x": "x * 2"});
    let e = v
        .push_level(
            inner.as_object().expect("o"),
            Clock::Layer,
            "layers[0].vars",
        )
        .expect_err("cycle");
    assert!(e.to_string().contains("cycle: x -> x"), "{e}");
}
