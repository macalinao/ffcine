//! A grade's raw ffmpeg filters (`extra`, after the grade) as GPU hooks: the colour filters the
//! prototype's edits use, parsed from ffmpeg's filter syntax (`name=k=v:k=v`, positional
//! arguments, comma-separated chains) and run as `PREOUTPUT` hooks in chain order, so nothing
//! falls back to the CPU.
//!
//! Supported: `eq` (contrast, brightness, saturation, gamma, `gamma_r/g/b`, `gamma_weight`), `hue`
//! (h, s, H, b), `colorbalance` (rs..bh, pl), `curves` (preset, master/red/green/blue/all point
//! lists, natural spline), `unsharp` (luma and chroma sizes and amounts), `vignette` (angle) and
//! `colorlevels` (per-channel input and output black and white points).
//! `eq`, `hue` and `unsharp` work on BT.601 limited-range YUV code values, as ffmpeg's
//! auto-inserted conversion from RGB gives them; `vignette` and `colorlevels` work on RGB.
//! [`hooks`] reports anything else and doesn't apply it; [`strict_hooks`] and [`filter_hook`]
//! (comp layers' `filters`) refuse it.

use crate::Hook;

/// One filter of a chain: its name and arguments, `(None, value)` for positional ones.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub name: String,
    pub args: Vec<(Option<String>, String)>,
}

impl Filter {
    /// The value of option `names[0]` (or an alias in `names`), or positional argument `pos`.
    fn get(&self, names: &[&str], pos: Option<usize>) -> Option<&str> {
        self.args
            .iter()
            .find(|(k, _)| k.as_deref().is_some_and(|k| names.contains(&k)))
            .or_else(|| pos.and_then(|p| self.args.iter().filter(|(k, _)| k.is_none()).nth(p)))
            .map(|(_, v)| v.as_str())
    }
}

/// Splits `s` at top-level `sep`, honouring `'...'` quoting and `\` escapes (kept for the next
/// level).
fn split(s: &str, sep: char) -> Vec<String> {
    let mut out = vec![String::new()];
    let (mut quoted, mut escaped) = (false, false);
    for c in s.chars() {
        let cur = out.last_mut().expect("non-empty");
        if escaped {
            cur.push(c);
            escaped = false;
        } else if c == '\\' {
            cur.push(c);
            escaped = true;
        } else if c == '\'' {
            cur.push(c);
            quoted = !quoted;
        } else if c == sep && !quoted {
            out.push(String::new());
        } else {
            cur.push(c);
        }
    }
    out
}

/// Removes one level of quoting and escaping.
fn unquote(s: &str) -> String {
    let mut out = String::new();
    let (mut quoted, mut escaped) = (false, false);
    for c in s.trim().chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' && !quoted {
            escaped = true;
        } else if c == '\'' {
            quoted = !quoted;
        } else {
            out.push(c);
        }
    }
    out
}

/// Parses a filter chain (`eq=contrast=1.1:saturation=0.9,hue=s=0`). Link labels (`[in]`) are
/// dropped; graphs (`;`) are an error.
pub fn parse(chain: &str) -> Result<Vec<Filter>, String> {
    if split(chain, ';').len() > 1 {
        return Err(format!("{chain:?}: only a linear chain is supported"));
    }
    let mut out = Vec::new();
    for f in split(chain, ',') {
        let mut f = f.trim();
        while let Some(rest) = f.strip_prefix('[') {
            f = rest.split_once(']').map_or("", |(_, r)| r).trim_start();
        }
        while f.ends_with(']') {
            f = f.rsplit_once('[').map_or("", |(l, _)| l).trim_end();
        }
        if f.is_empty() {
            continue;
        }
        let (name, args) = f.split_once('=').unwrap_or((f, ""));
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("{f:?}: not a filter"));
        }
        let args = if args.is_empty() {
            Vec::new()
        } else {
            split(args, ':')
                .into_iter()
                .map(|a| {
                    // `k=v` unless the `=` is quoted or escaped.
                    match split(&a, '=').as_slice() {
                        [k, _, ..] if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                            let v = &a[k.len() + 1..];
                            (Some(k.clone()), unquote(v))
                        }
                        _ => (None, unquote(&a)),
                    }
                })
                .collect()
        };
        out.push(Filter {
            name: name.to_owned(),
            args,
        });
    }
    Ok(out)
}

/// Evaluates a constant ffmpeg expression: numbers, `PI`, `PHI`, `E`, `+ - * /`, parentheses.
pub fn eval(s: &str) -> Option<f64> {
    struct P<'a> {
        s: &'a [u8],
        i: usize,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
                self.i += 1;
            }
        }
        fn eat(&mut self, c: u8) -> bool {
            self.ws();
            let ok = self.s.get(self.i) == Some(&c);
            self.i += usize::from(ok);
            ok
        }
        fn sum(&mut self) -> Option<f64> {
            let mut v = self.product()?;
            loop {
                if self.eat(b'+') {
                    v += self.product()?;
                } else if self.eat(b'-') {
                    v -= self.product()?;
                } else {
                    return Some(v);
                }
            }
        }
        fn product(&mut self) -> Option<f64> {
            let mut v = self.unary()?;
            loop {
                if self.eat(b'*') {
                    v *= self.unary()?;
                } else if self.eat(b'/') {
                    v /= self.unary()?;
                } else {
                    return Some(v);
                }
            }
        }
        fn unary(&mut self) -> Option<f64> {
            if self.eat(b'-') {
                return self.unary().map(|v| -v);
            }
            if self.eat(b'+') {
                return self.unary();
            }
            if self.eat(b'(') {
                let v = self.sum()?;
                return self.eat(b')').then_some(v);
            }
            self.ws();
            let start = self.i;
            while self
                .s
                .get(self.i)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'.')
                || (self.i > start
                    && matches!(self.s.get(self.i), Some(b'+' | b'-'))
                    && matches!(self.s.get(self.i - 1), Some(b'e' | b'E'))
                    && self.s[start].is_ascii_digit())
            {
                self.i += 1;
            }
            let tok = std::str::from_utf8(&self.s[start..self.i]).ok()?;
            match tok {
                "PI" => Some(std::f64::consts::PI),
                "E" => Some(std::f64::consts::E),
                "PHI" => Some(1.618_033_988_749_895),
                _ => tok.parse().ok(),
            }
        }
    }
    let mut p = P {
        s: s.trim().as_bytes(),
        i: 0,
    };
    let v = p.sum()?;
    p.ws();
    (p.i == p.s.len() && v.is_finite()).then_some(v)
}

/// The hooks for a grade's `extra` filter strings, in order, and what isn't applied.
pub fn hooks(extra: &[String]) -> (Vec<Hook>, Vec<String>) {
    let mut hooks = Vec::new();
    let mut skipped = Vec::new();
    for chain in extra {
        match parse(chain) {
            Ok(fs) => {
                for f in fs {
                    match hook(&f) {
                        Ok((h, warn)) => {
                            hooks.extend(h);
                            skipped.extend(warn);
                        }
                        Err(e) => skipped.push(e),
                    }
                }
            }
            Err(e) => skipped.push(format!("extra filter {e}")),
        }
    }
    (hooks, skipped)
}

/// The hooks for filter chains, in order; an error for anything [`hooks`] would skip or warn
/// about (comp layers' `filters`: never silently dropped).
pub fn strict_hooks(chains: &[String]) -> Result<Vec<Hook>, String> {
    let mut out = Vec::new();
    for chain in chains {
        for f in parse(chain).map_err(|e| format!("filter {e}"))? {
            out.push(filter_hook(&f)?);
        }
    }
    Ok(out)
}

/// One filter's hook; an error if it isn't supported, or any of its options isn't.
pub fn filter_hook(f: &Filter) -> Result<Hook, String> {
    let (h, warn) = hook(f)?;
    if let Some(w) = warn.into_iter().next() {
        return Err(w);
    }
    h.ok_or_else(|| format!("filter {}: not applied", f.name))
}

/// Options of each supported filter, in ffmpeg's (positional) order, with aliases.
const EQ: &[&[&str]] = &[
    &["contrast"],
    &["brightness"],
    &["saturation"],
    &["gamma"],
    &["gamma_r"],
    &["gamma_g"],
    &["gamma_b"],
    &["gamma_weight"],
];
const HUE: &[&[&str]] = &[&["h"], &["s"], &["H"], &["b"]];
const COLORBALANCE: &[&[&str]] = &[
    &["rs"],
    &["gs"],
    &["bs"],
    &["rm"],
    &["gm"],
    &["bm"],
    &["rh"],
    &["gh"],
    &["bh"],
    &["pl"],
];
const CURVES: &[&[&str]] = &[
    &["preset"],
    &["master", "m"],
    &["red", "r"],
    &["green", "g"],
    &["blue", "b"],
    &["all"],
];
const UNSHARP: &[&[&str]] = &[
    &["luma_msize_x", "lx"],
    &["luma_msize_y", "ly"],
    &["luma_amount", "la"],
    &["chroma_msize_x", "cx"],
    &["chroma_msize_y", "cy"],
    &["chroma_amount", "ca"],
];
const VIGNETTE: &[&[&str]] = &[&["angle", "a"]];
const COLORLEVELS: &[&[&str]] = &[
    &["rimin"],
    &["gimin"],
    &["bimin"],
    &["aimin"],
    &["rimax"],
    &["gimax"],
    &["bimax"],
    &["aimax"],
    &["romin"],
    &["gomin"],
    &["bomin"],
    &["aomin"],
    &["romax"],
    &["gomax"],
    &["bomax"],
    &["aomax"],
    &["preserve"],
];

/// Numeric option `i` of `opts` with default `d`; an error if it isn't a constant.
fn num(f: &Filter, opts: &[&[&str]], i: usize, d: f64) -> Result<f64, String> {
    f.get(opts[i], Some(i)).map_or(Ok(d), |v| {
        eval(v).ok_or_else(|| {
            format!(
                "extra filter {}: {}={v} is not a constant",
                f.name, opts[i][0]
            )
        })
    })
}

fn hook(f: &Filter) -> Result<(Option<Hook>, Vec<String>), String> {
    let opts = match f.name.as_str() {
        "eq" => EQ,
        "hue" => HUE,
        "colorbalance" => COLORBALANCE,
        "curves" => CURVES,
        "unsharp" => UNSHARP,
        "vignette" => VIGNETTE,
        "colorlevels" => COLORLEVELS,
        n => return Err(format!("extra filter {n} not supported")),
    };
    let mut warn = Vec::new();
    let positional = f.args.iter().filter(|(k, _)| k.is_none()).count();
    if positional > opts.len() {
        warn.push(format!("extra filter {}: too many arguments", f.name));
    }
    for (k, _) in &f.args {
        if let Some(k) = k
            && !opts.iter().any(|o| o.contains(&k.as_str()))
        {
            warn.push(format!("extra filter {}: option {k} not supported", f.name));
        }
    }
    let n = |i: usize, d: f64| num(f, opts, i, d);
    let hook = |source: String, params: Vec<(&str, f64)>| Hook {
        name: format!("extra {}", f.name),
        source,
        params: params.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        tracks: Vec::new(),
    };
    let h = match f.name.as_str() {
        "eq" => {
            let (gamma, gr, gg, gb) = (n(3, 1.0)?, n(4, 1.0)?, n(5, 1.0)?, n(6, 1.0)?);
            hook(
                eq(),
                vec![
                    ("contrast", n(0, 1.0)?),
                    ("brightness", n(1, 0.0)?),
                    ("saturation", n(2, 1.0)?),
                    // vf_eq's per-plane gammas.
                    ("gamma_y", gamma * gg),
                    ("gamma_u", (gb / gg).sqrt()),
                    ("gamma_v", (gr / gg).sqrt()),
                    ("gamma_weight", n(7, 1.0)?),
                ],
            )
        }
        "hue" => {
            let h = match f.get(opts[2], None) {
                Some(_) => n(2, 0.0)?,
                None => n(0, 0.0)?.to_radians(),
            };
            hook(
                hue(),
                vec![("hue", h), ("sat", n(1, 1.0)?), ("bright", n(3, 0.0)?)],
            )
        }
        "colorbalance" => {
            let names = ["rs", "gs", "bs", "rm", "gm", "bm", "rh", "gh", "bh", "pl"];
            let mut p = Vec::new();
            for (i, k) in names.iter().enumerate() {
                p.push((*k, n(i, 0.0)?));
            }
            hook(colorbalance().to_owned(), p)
        }
        "curves" => {
            let c = Curves::from_filter(f)?;
            hook(curves_hook(&c), Vec::new())
        }
        "unsharp" => {
            let size = |i: usize| -> Result<u32, String> {
                let v = n(i, 5.0)?;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let s = v.round().clamp(3.0, 23.0) as u32;
                if s % 2 == 0 {
                    return Err(format!("extra filter unsharp: size {s} must be odd"));
                }
                Ok(s)
            };
            let sizes = [size(0)?, size(1)?, size(3)?, size(4)?];
            hook(
                unsharp(sizes),
                vec![("luma", n(2, 1.0)?), ("chroma", n(5, 0.0)?)],
            )
        }
        "vignette" => hook(
            vignette().to_owned(),
            vec![("angle", n(0, std::f64::consts::PI / 5.0)?)],
        ),
        "colorlevels" => {
            // ffmpeg's defaults; negative input points are its per-frame auto levels.
            let mut p = Vec::new();
            for (i, (k, d)) in [("imin", 0.0), ("imax", 1.0), ("omin", 0.0), ("omax", 1.0)]
                .into_iter()
                .enumerate()
            {
                for (c, ch) in ["r", "g", "b"].into_iter().enumerate() {
                    let v = n(4 * i + c, d)?;
                    if i < 2 && v < 0.0 {
                        return Err(format!(
                            "extra filter colorlevels: {ch}{k}={v} (auto levels) is not supported"
                        ));
                    }
                    p.push((format!("{ch}{k}"), v));
                }
                if (n(4 * i + 3, d)? - d).abs() > 0.0 {
                    warn.push(format!(
                        "extra filter colorlevels: option a{k} not supported (alpha)"
                    ));
                }
            }
            if f.get(opts[16], Some(16))
                .is_some_and(|v| v != "0" && v != "none")
            {
                warn.push("extra filter colorlevels: option preserve not supported".into());
            }
            Hook {
                name: "extra colorlevels".into(),
                source: colorlevels().to_owned(),
                params: p,
                tracks: Vec::new(),
            }
        }
        _ => unreachable!("checked above"),
    };
    Ok((Some(h), warn))
}

/// BT.601 YUV <-> RGB in the hooks below (`ycc`: Y, Cb, Cr as limited-range code values / 255).
const YUV: &str = r"
#define TO_YCC(v) vec3((16.0 + 219.0 * dot(v, vec3(0.299, 0.587, 0.114))) / 255.0, \
    (128.0 + 224.0 * (v.b - dot(v, vec3(0.299, 0.587, 0.114))) / 1.772) / 255.0, \
    (128.0 + 224.0 * (v.r - dot(v, vec3(0.299, 0.587, 0.114))) / 1.402) / 255.0)
#define FROM_YCC(q) (vec3((q.x * 255.0 - 16.0) / 219.0) + mat3(0.0, -0.344136, 1.772, \
    1.402, -0.714136, 0.0, 0.0, 0.0, 0.0) * vec3((q.yz * 255.0 - 128.0) / 224.0, 0.0))
";

/// ffmpeg's `eq` on BT.601 code values: per plane `v = k (v − ½) + ½ + b` (luma: contrast and
/// brightness; chroma: saturation), then `v (1 − w) + v^(1/gamma) w`, 0 below 0, 1 above.
pub fn eq() -> String {
    format!(
        r"//!PARAM contrast
//!TYPE float
1.0
//!PARAM brightness
//!TYPE float
0.0
//!PARAM saturation
//!TYPE float
1.0
//!PARAM gamma_y
//!TYPE float
//!MINIMUM 0.01
1.0
//!PARAM gamma_u
//!TYPE float
//!MINIMUM 0.01
1.0
//!PARAM gamma_v
//!TYPE float
//!MINIMUM 0.01
1.0
//!PARAM gamma_weight
//!TYPE float
1.0
//!HOOK PREOUTPUT
//!BIND HOOKED
//!DESC ffcine extra eq
{YUV}
vec4 hook() {{
    vec4 c = HOOKED_texOff(vec2(0.0));
    vec3 q = TO_YCC(clamp(c.rgb, 0.0, 1.0));
    vec3 x = vec3(contrast, saturation, saturation) * (q - 0.5) + 0.5 + vec3(brightness, 0.0, 0.0);
    vec3 g = vec3(gamma_y, gamma_u, gamma_v);
    vec3 p = max(x, 0.0);
    p = p * (1.0 - gamma_weight) + pow(p, 1.0 / g) * gamma_weight;
    q = clamp(p, 0.0, 1.0) * vec3(greaterThan(x, vec3(0.0)));
    c.rgb = clamp(FROM_YCC(q), 0.0, 1.0);
    return c;
}}
"
    )
}

/// ffmpeg's `hue`: chroma rotated by `hue` radians and scaled by `sat`, luma code values moved
/// by `bright` × 25.5 / 255.
pub fn hue() -> String {
    format!(
        r"//!PARAM hue
//!TYPE float
0.0
//!PARAM sat
//!TYPE float
1.0
//!PARAM bright
//!TYPE float
0.0
//!HOOK PREOUTPUT
//!BIND HOOKED
//!DESC ffcine extra hue
{YUV}
vec4 hook() {{
    vec4 c = HOOKED_texOff(vec2(0.0));
    vec3 q = TO_YCC(clamp(c.rgb, 0.0, 1.0));
    vec2 uv = q.yz - 128.0 / 255.0;
    float s = sin(hue) * sat, k = cos(hue) * sat;
    q.yz = clamp(vec2(k * uv.x - s * uv.y, s * uv.x + k * uv.y) + 128.0 / 255.0, 0.0, 1.0);
    q.x = clamp(q.x + bright * 0.1, 0.0, 1.0);
    c.rgb = clamp(FROM_YCC(q), 0.0, 1.0);
    return c;
}}
"
    )
}

/// ffmpeg's `colorbalance`: shadows/midtones/highlights offsets per channel weighted by the
/// input's `max + min` (ffmpeg's weights, see [`crate::shaders::look`]), and with `pl` ffmpeg's
/// lightness preservation (HSL, at the input's lightness).
pub fn colorbalance() -> &'static str {
    r"//!PARAM rs
//!TYPE float
0.0
//!PARAM gs
//!TYPE float
0.0
//!PARAM bs
//!TYPE float
0.0
//!PARAM rm
//!TYPE float
0.0
//!PARAM gm
//!TYPE float
0.0
//!PARAM bm
//!TYPE float
0.0
//!PARAM rh
//!TYPE float
0.0
//!PARAM gh
//!TYPE float
0.0
//!PARAM bh
//!TYPE float
0.0
//!PARAM pl
//!TYPE float
0.0
//!HOOK PREOUTPUT
//!BIND HOOKED
//!DESC ffcine extra colorbalance
vec4 hook() {
    vec4 c = HOOKED_texOff(vec2(0.0));
    vec3 v = clamp(c.rgb, 0.0, 1.0);
    // vf_colorbalance weighs by max + min (twice the HSL lightness).
    float m2 = max(v.r, max(v.g, v.b)) + min(v.r, min(v.g, v.b));
    float l = 0.5 * m2;
    const float a = 4.0, b = 0.333, k = 0.7;
    float ws = clamp((b - m2) * a + 0.5, 0.0, 1.0) * k;
    float wm = clamp((m2 - b) * a + 0.5, 0.0, 1.0) * clamp((1.0 - m2 - b) * a + 0.5, 0.0, 1.0) * k;
    float wh = clamp((m2 + b - 1.0) * a + 0.5, 0.0, 1.0) * k;
    vec3 o = clamp(v + ws * vec3(rs, gs, bs) + wm * vec3(rm, gm, bm) + wh * vec3(rh, gh, bh),
        0.0, 1.0);
    if (pl > 0.5) {
        // vf_colorbalance's preservel: the result's HSL hue, its saturation over the input's
        // lightness (0 if a channel is at 0 or 1), at the input's lightness.
        float mx = max(o.r, max(o.g, o.b)), mn = min(o.r, min(o.g, o.b));
        float h = 0.0;
        if (mx > mn) {
            if (mx == o.r)
                h = 60.0 * ((o.g - o.b) / (mx - mn));
            else if (mx == o.g)
                h = 60.0 * (2.0 + (o.b - o.r) / (mx - mn));
            else
                h = 60.0 * (4.0 + (o.r - o.g) / (mx - mn));
        }
        if (h < 0.0)
            h += 360.0;
        float s = mx == 1.0 || mn == 0.0 ? 0.0 : (mx - mn) / (1.0 - abs(2.0 * l - 1.0));
        float q = s * min(l, 1.0 - l);
        vec3 kk = mod(vec3(0.0, 8.0, 4.0) + h / 30.0, 12.0);
        o = clamp(l - q * max(min(min(kk - 3.0, 9.0 - kk), 1.0), -1.0), 0.0, 1.0);
    }
    c.rgb = o;
    return c;
}
"
}

/// ffmpeg's `unsharp` with luma `lx x ly` and chroma `cx x cy` matrices (`sizes`), the binomial
/// blur ffmpeg's repeated `[1 2 1]` passes give, amounts `luma` and `chroma` as params: luma on
/// BT.601 Y (the same delta on R, G and B), chroma on the rest of the pixel.
pub fn unsharp(sizes: [u32; 4]) -> String {
    let weights = |n: u32| -> String {
        let mut w = vec![1.0f64];
        for _ in 1..n {
            let mut next = vec![1.0; w.len() + 1];
            for i in 1..w.len() {
                next[i] = w[i - 1] + w[i];
            }
            w = next;
        }
        let sum: f64 = w.iter().sum();
        w.iter()
            .map(|x| format!("{:.8}", x / sum))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let [lx, ly, cx, cy] = sizes;
    let (lw_x, lw_y, cw_x, cw_y) = (weights(lx), weights(ly), weights(cx), weights(cy));
    let (lrx, lry, crx, cry) = (lx / 2, ly / 2, cx / 2, cy / 2);
    format!(
        r"//!PARAM luma
//!TYPE float
1.0
//!PARAM chroma
//!TYPE float
0.0
//!HOOK PREOUTPUT
//!BIND HOOKED
//!DESC ffcine extra unsharp {lx}x{ly} {cx}x{cy}
vec4 hook() {{
    const vec3 k = vec3(0.299, 0.587, 0.114);
    const float lwx[{lx}] = float[{lx}]({lw_x});
    const float lwy[{ly}] = float[{ly}]({lw_y});
    const float cwx[{cx}] = float[{cx}]({cw_x});
    const float cwy[{cy}] = float[{cy}]({cw_y});
    vec4 c = HOOKED_texOff(vec2(0.0));
    float y = dot(c.rgb, k);
    vec3 d = c.rgb - y;
    float yb = 0.0;
    if (luma != 0.0)
        for (int j = -{lry}; j <= {lry}; j++)
            for (int i = -{lrx}; i <= {lrx}; i++)
                yb += lwx[i + {lrx}] * lwy[j + {lry}]
                    * dot(HOOKED_texOff(vec2(float(i), float(j))).rgb, k);
    vec3 db = vec3(0.0);
    if (chroma != 0.0)
        for (int j = -{cry}; j <= {cry}; j++)
            for (int i = -{crx}; i <= {crx}; i++) {{
                vec3 s = HOOKED_texOff(vec2(float(i), float(j))).rgb;
                db += cwx[i + {crx}] * cwy[j + {cry}] * (s - dot(s, k));
            }}
    if (luma != 0.0)
        c.rgb += luma * (y - yb);
    if (chroma != 0.0)
        c.rgb += chroma * (d - db);
    return c;
}}
"
    )
}

/// ffmpeg's `vignette`: RGB times `cos(angle · d)^4`, `d` the distance from the middle over the
/// half diagonal.
pub fn vignette() -> &'static str {
    r"//!PARAM angle
//!TYPE float
0.6283185
//!HOOK PREOUTPUT
//!BIND HOOKED
//!DESC ffcine extra vignette
vec4 hook() {
    vec4 c = HOOKED_texOff(vec2(0.0));
    vec2 h = 0.5 * HOOKED_size;
    float d = length(HOOKED_pos * HOOKED_size - h) / length(h);
    float k = d > 1.0 ? 0.0 : cos(angle * d);
    c.rgb *= k * k * k * k;
    return c;
}
"
}

/// ffmpeg's `colorlevels` on RGB: per channel `(v − imin) / (imax − imin) · (omax − omin) +
/// omin`, clamped to 0..1.
pub fn colorlevels() -> &'static str {
    r"//!PARAM rimin
//!TYPE float
0.0
//!PARAM gimin
//!TYPE float
0.0
//!PARAM bimin
//!TYPE float
0.0
//!PARAM rimax
//!TYPE float
1.0
//!PARAM gimax
//!TYPE float
1.0
//!PARAM bimax
//!TYPE float
1.0
//!PARAM romin
//!TYPE float
0.0
//!PARAM gomin
//!TYPE float
0.0
//!PARAM bomin
//!TYPE float
0.0
//!PARAM romax
//!TYPE float
1.0
//!PARAM gomax
//!TYPE float
1.0
//!PARAM bomax
//!TYPE float
1.0
//!HOOK PREOUTPUT
//!BIND HOOKED
//!DESC ffcine extra colorlevels
vec4 hook() {
    vec4 c = HOOKED_texOff(vec2(0.0));
    vec3 imin = vec3(rimin, gimin, bimin);
    vec3 imax = vec3(rimax, gimax, bimax);
    vec3 omin = vec3(romin, gomin, bomin);
    vec3 omax = vec3(romax, gomax, bomax);
    vec3 k = (omax - omin) / max(imax - imin, vec3(1e-6));
    c.rgb = clamp((clamp(c.rgb, 0.0, 1.0) - imin) * k + omin, 0.0, 1.0);
    return c;
}
"
}

/// ffmpeg's `curves` presets: (red, green, blue, master) point lists.
const PRESETS: &[(&str, [Option<&str>; 4])] = &[
    (
        "color_negative",
        [
            Some("0.129/1 0.466/0.498 0.725/0"),
            Some("0.109/1 0.301/0.498 0.517/0"),
            Some("0.098/1 0.235/0.498 0.423/0"),
            None,
        ],
    ),
    (
        "cross_process",
        [
            Some("0/0 0.25/0.156 0.501/0.501 0.686/0.745 1/1"),
            Some("0/0 0.25/0.188 0.38/0.501 0.745/0.815 1/0.815"),
            Some("0/0 0.231/0.094 0.709/0.874 1/1"),
            None,
        ],
    ),
    ("darker", [None, None, None, Some("0/0 0.5/0.4 1/1")]),
    (
        "increase_contrast",
        [
            None,
            None,
            None,
            Some("0/0 0.149/0.066 0.831/0.905 0.905/0.98 1/1"),
        ],
    ),
    ("lighter", [None, None, None, Some("0/0 0.4/0.5 1/1")]),
    (
        "linear_contrast",
        [None, None, None, Some("0/0 0.305/0.286 0.694/0.713 1/1")],
    ),
    (
        "medium_contrast",
        [None, None, None, Some("0/0 0.286/0.219 0.639/0.643 1/1")],
    ),
    ("negative", [None, None, None, Some("0/1 1/0")]),
    (
        "strong_contrast",
        [
            None,
            None,
            None,
            Some("0/0 0.301/0.196 0.592/0.6 0.686/0.737 1/1"),
        ],
    ),
    (
        "vintage",
        [
            Some("0/0.11 0.42/0.51 1/0.95"),
            Some("0/0 0.50/0.48 1/1"),
            Some("0/0.22 0.49/0.44 1/0.8"),
            None,
        ],
    ),
];

/// Samples per channel of a [`Curves`] table.
pub const CURVE_SAMPLES: usize = 256;

/// `curves` as per-channel tables over `0..=1` with the master curve folded in.
#[derive(Debug, Clone, PartialEq)]
pub struct Curves {
    pub rgb: [Vec<f32>; 3],
}

impl Curves {
    fn from_filter(f: &Filter) -> Result<Self, String> {
        let bad = |e: String| format!("extra filter curves: {e}");
        let preset = match f.get(CURVES[0], Some(0)) {
            None | Some("none") => [None; 4],
            Some(p) => {
                PRESETS
                    .iter()
                    .find(|(n, _)| *n == p)
                    .ok_or_else(|| bad(format!("unknown preset {p}")))?
                    .1
            }
        };
        let all = f.get(CURVES[5], None);
        let mut lists = [None; 4];
        for (c, opt) in [(0, 2), (1, 3), (2, 4), (3, 1)] {
            lists[c] = f
                .get(CURVES[opt], None)
                .or(if c < 3 { all } else { None })
                .or(preset[c]);
        }
        let table = |s: Option<&str>| -> Result<Vec<f32>, String> {
            let pts = points(s.unwrap_or("0/0 1/1")).map_err(bad)?;
            Ok(spline(&pts, CURVE_SAMPLES))
        };
        let master = table(lists[3])?;
        let mut rgb: [Vec<f32>; 3] = Default::default();
        for (c, out) in rgb.iter_mut().enumerate() {
            *out = table(lists[c])?
                .into_iter()
                .map(|v| lerp_table(&master, v))
                .collect();
        }
        Ok(Self { rgb })
    }
}

/// `x/y x/y ...` key points, x strictly increasing.
fn points(s: &str) -> Result<Vec<(f64, f64)>, String> {
    let mut v: Vec<(f64, f64)> = Vec::new();
    for p in s.split_whitespace() {
        let (x, y) = p
            .split_once('/')
            .ok_or_else(|| format!("point {p:?} is not x/y"))?;
        let (x, y): (f64, f64) = (
            x.parse().map_err(|_| format!("point {p:?}"))?,
            y.parse().map_err(|_| format!("point {p:?}"))?,
        );
        if v.last().is_some_and(|l| l.0 >= x) {
            return Err(format!("points {s:?} must have increasing x"));
        }
        v.push((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
    }
    if v.is_empty() {
        return Err("no points".into());
    }
    Ok(v)
}

/// A natural cubic spline through `pts` sampled at `n` points over `0..=1`, flat outside the
/// first and last point, clipped to `0..=1` (ffmpeg's `interp=natural`).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names
)]
fn spline(pts: &[(f64, f64)], n: usize) -> Vec<f32> {
    let k = pts.len();
    // Second derivatives, natural ends (Thomas algorithm).
    let mut m = vec![0.0; k];
    if k > 2 {
        let h: Vec<f64> = pts.windows(2).map(|w| w[1].0 - w[0].0).collect();
        let mut c = vec![0.0; k];
        let mut d = vec![0.0; k];
        for i in 1..k - 1 {
            let a = h[i - 1];
            let b = 2.0 * (h[i - 1] + h[i]);
            let cc = h[i];
            let r = 6.0 * ((pts[i + 1].1 - pts[i].1) / h[i] - (pts[i].1 - pts[i - 1].1) / h[i - 1]);
            let den = b - a * c[i - 1];
            c[i] = cc / den;
            d[i] = (r - a * d[i - 1]) / den;
        }
        for i in (1..k - 1).rev() {
            m[i] = d[i] - c[i] * m[i + 1];
        }
    }
    (0..n)
        .map(|s| {
            let x = s as f64 / (n - 1) as f64;
            let y = if k == 1 || x <= pts[0].0 {
                pts[0].1
            } else if x >= pts[k - 1].0 {
                pts[k - 1].1
            } else {
                let i = pts.windows(2).position(|w| x < w[1].0).unwrap_or(k - 2);
                let (x0, y0, x1, y1) = (pts[i].0, pts[i].1, pts[i + 1].0, pts[i + 1].1);
                let h = x1 - x0;
                let (a, b) = ((x1 - x) / h, (x - x0) / h);
                a * y0
                    + b * y1
                    + ((a * a * a - a) * m[i] + (b * b * b - b) * m[i + 1]) * h * h / 6.0
            };
            y.clamp(0.0, 1.0) as f32
        })
        .collect()
}

/// Linear lookup of `x` in `0..=1` in a table sampled evenly over `0..=1`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]
fn lerp_table(t: &[f32], x: f32) -> f32 {
    let p = x.clamp(0.0, 1.0) * (t.len() - 1) as f32;
    let i = (p.floor() as usize).min(t.len() - 2);
    let f = p - i as f32;
    t[i] + (t[i + 1] - t[i]) * f
}

/// The [`Curves`] tables as a hook (constant arrays, linear between samples).
fn curves_hook(c: &Curves) -> String {
    let arr = |v: &[f32]| {
        v.iter()
            .map(|x| format!("{x:.6}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let n = CURVE_SAMPLES;
    format!(
        r"//!HOOK PREOUTPUT
//!BIND HOOKED
//!DESC ffcine extra curves
const float cr[{n}] = float[{n}]({});
const float cg[{n}] = float[{n}]({});
const float cb[{n}] = float[{n}]({});
vec4 hook() {{
    vec4 c = HOOKED_texOff(vec2(0.0));
    vec3 p = clamp(c.rgb, 0.0, 1.0) * {last}.0;
    ivec3 i = min(ivec3(p), ivec3({n2}));
    vec3 f = p - vec3(i);
    c.rgb = vec3(mix(cr[i.r], cr[i.r + 1], f.r), mix(cg[i.g], cg[i.g + 1], f.g),
        mix(cb[i.b], cb[i.b + 1], f.b));
    return c;
}}
",
        arr(&c.rgb[0]),
        arr(&c.rgb[1]),
        arr(&c.rgb[2]),
        last = n - 1,
        n2 = n - 2,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(h: &Hook, k: &str) -> f64 {
        h.params
            .iter()
            .find(|(n, _)| n == k)
            .unwrap_or_else(|| panic!("{k} in {:?}", h.params))
            .1
    }

    #[test]
    fn parses_ffmpeg_syntax() {
        let f = parse("eq=1.2:0.05:saturation=0.9 , hue=h=30:s=1.1,[x]unsharp[y]").unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].name, "eq");
        assert_eq!(
            f[0].args,
            [
                (None, "1.2".to_owned()),
                (None, "0.05".to_owned()),
                (Some("saturation".to_owned()), "0.9".to_owned())
            ]
        );
        assert_eq!(f[0].get(EQ[1], Some(1)), Some("0.05"));
        assert_eq!(f[1].get(HUE[0], Some(0)), Some("30"));
        assert_eq!(f[2].name, "unsharp");
        assert_eq!(f[2].args, []);
        // Quotes keep ':' and ',' inside a value; escapes too.
        let f = parse(r"curves=r='0/0 0.5/0.6 1/1':g=0/0\ 1/0.9,eq=gamma=1.1").unwrap();
        assert_eq!(f[0].get(CURVES[2], None), Some("0/0 0.5/0.6 1/1"));
        assert_eq!(f[0].get(CURVES[3], None), Some("0/0 1/0.9"));
        assert_eq!(f[1].name, "eq");
        let f = parse("drawtext=text='a:b, c'").unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].get(&["text"], None), Some("a:b, c"));
        assert!(parse("[a]split[b];[b]null").is_err());
    }

    #[test]
    fn evaluates_constants() {
        assert_eq!(eval("1.5"), Some(1.5));
        assert_eq!(eval("-2e-1"), Some(-0.2));
        assert!((eval("PI/4").unwrap() - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
        assert_eq!(eval("2*(1+0.5)"), Some(3.0));
        assert_eq!(eval("t*2"), None);
        assert_eq!(eval("1+"), None);
    }

    #[test]
    fn hook_params() {
        let (h, warn) = hooks(&[
            "eq=contrast=1.1:brightness=0.02:saturation=1.2:gamma=0.9:gamma_r=1.2".into(),
            "hue=h=90:s=0.5:b=1,colorbalance=rs=0.1:bh=-0.05:pl=1".into(),
            "unsharp=7:5:0.8,vignette=PI/4".into(),
        ]);
        assert_eq!(warn, Vec::<String>::new());
        let names: Vec<_> = h.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "extra eq",
                "extra hue",
                "extra colorbalance",
                "extra unsharp",
                "extra vignette"
            ]
        );
        assert_eq!(param(&h[0], "contrast"), 1.1);
        assert_eq!(param(&h[0], "brightness"), 0.02);
        assert_eq!(param(&h[0], "saturation"), 1.2);
        assert_eq!(param(&h[0], "gamma_y"), 0.9);
        assert!((param(&h[0], "gamma_v") - 1.2f64.sqrt()).abs() < 1e-12);
        assert_eq!(param(&h[0], "gamma_u"), 1.0);
        assert!((param(&h[1], "hue") - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        assert_eq!(param(&h[1], "sat"), 0.5);
        assert_eq!(param(&h[1], "bright"), 1.0);
        assert_eq!(param(&h[2], "rs"), 0.1);
        assert_eq!(param(&h[2], "bh"), -0.05);
        assert_eq!(param(&h[2], "pl"), 1.0);
        assert_eq!(param(&h[2], "gm"), 0.0);
        assert_eq!(param(&h[3], "luma"), 0.8);
        assert_eq!(param(&h[3], "chroma"), 0.0);
        assert!(h[3].source.contains("unsharp 7x5 5x5"));
        // 7 taps: binomial 1 6 15 20 15 6 1 over 64.
        assert!(h[3].source.contains("0.31250000"));
        assert!((param(&h[4], "angle") - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
        // Every hook declares the params it's given.
        for hk in &h {
            for (k, _) in &hk.params {
                assert!(hk.source.contains(&format!("//!PARAM {k}\n")), "{k}");
            }
        }
        // H (radians) wins over h.
        let (h, _) = hooks(&["hue=H=1.0:h=90".into()]);
        assert_eq!(param(&h[0], "hue"), 1.0);
    }

    #[test]
    fn reports_what_isnt_applied() {
        let (h, warn) = hooks(&[
            "hqdn3d=4,eq=contrast=1.1:eval=frame".into(),
            "hue=h=t*10".into(),
            "a;b".into(),
        ]);
        assert_eq!(h.len(), 1);
        assert_eq!(
            warn,
            [
                "extra filter hqdn3d not supported",
                "extra filter eq: option eval not supported",
                "extra filter hue: h=t*10 is not a constant",
                "extra filter \"a;b\": only a linear chain is supported",
            ]
        );
    }

    #[test]
    fn colorlevels_params() {
        let h =
            strict_hooks(&["colorlevels=rimin=0.1:gimin=0.05:romax=0.9:bimax=0.8".into()]).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(param(&h[0], "rimin"), 0.1);
        assert_eq!(param(&h[0], "gimin"), 0.05);
        assert_eq!(param(&h[0], "bimin"), 0.0);
        assert_eq!(param(&h[0], "bimax"), 0.8);
        assert_eq!(param(&h[0], "romax"), 0.9);
        assert_eq!(param(&h[0], "gomax"), 1.0);
        for (k, _) in &h[0].params {
            assert!(h[0].source.contains(&format!("//!PARAM {k}\n")), "{k}");
        }
        // Positional: rimin, gimin, bimin, aimin, rimax ...
        let h = strict_hooks(&["colorlevels=0.2:0:0:0:0.9".into()]).unwrap();
        assert_eq!(param(&h[0], "rimin"), 0.2);
        assert_eq!(param(&h[0], "rimax"), 0.9);
        for bad in [
            "colorlevels=rimin=-1",
            "colorlevels=aomax=0.5",
            "colorlevels=preserve=lum",
        ] {
            assert!(strict_hooks(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn strict_refuses_what_hooks_skip() {
        assert!(strict_hooks(&["eq=contrast=1.1".into()]).is_ok());
        for bad in [
            "hqdn3d=4",
            "eq=contrast=1.1:eval=frame",
            "hue=h=t*10",
            "a;b",
        ] {
            assert!(strict_hooks(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn curves_tables() {
        // Two points: a straight line.
        let t = spline(&[(0.0, 0.0), (1.0, 0.5)], 5);
        assert_eq!(t, [0.0, 0.125, 0.25, 0.375, 0.5]);
        // Through every key point, flat outside them.
        let pts = points("0.1/0.2 0.4/0.5 0.9/0.8").unwrap();
        let t = spline(&pts, 11);
        assert!((t[0] - 0.2).abs() < 1e-6 && (t[1] - 0.2).abs() < 1e-6);
        assert!((t[4] - 0.5).abs() < 1e-6);
        assert!((t[9] - 0.8).abs() < 1e-6 && (t[10] - 0.8).abs() < 1e-6);
        // The natural spline bends between points (a symmetric S here would be a line).
        let s = spline(&points("0/0 0.5/0.7 1/1").unwrap(), 5);
        assert!(s[1] > 0.35 && s[1] < 0.5, "{s:?}");

        let (h, w) = hooks(&["curves=preset=lighter".into()]);
        assert!(w.is_empty(), "{w:?}");
        assert!(h[0].source.contains("const float cr[256]"));
        let f = &parse("curves=lighter").unwrap()[0];
        let c = Curves::from_filter(f).unwrap();
        // Master 0.4 -> 0.5 on every channel.
        let at = |v: &[f32], x: f32| lerp_table(v, x);
        for ch in &c.rgb {
            assert!((at(ch, 0.4) - 0.5).abs() < 2e-3, "{}", at(ch, 0.4));
            assert!(at(ch, 0.0).abs() < 1e-6 && (at(ch, 1.0) - 1.0).abs() < 1e-6);
        }
        // Explicit points override the preset's; `all` fills unset channels.
        let f = &parse("curves=vintage:r=0/0\\ 1/1:all=0/1\\ 1/0").unwrap()[0];
        let c = Curves::from_filter(f).unwrap();
        assert!((at(&c.rgb[0], 0.3) - 0.3).abs() < 1e-3);
        assert!((at(&c.rgb[1], 0.3) - 0.7).abs() < 1e-3);
        assert!(hooks(&["curves=preset=nope".into()]).1[0].contains("unknown preset"));
    }
}
