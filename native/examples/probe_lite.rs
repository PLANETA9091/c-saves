//! Probe (v0.9.13 gate harness): measures fastnbt parse of the sections list
//! into (a) the owned Value tree (old scanp path) vs (b) the typed borrowed
//! structs the lite path uses (Cow strings, no per-entry HashMap), then
//! asserts per section: semantic equality, hash_lite == structural_hash
//! (stripped Value) and IDENTICAL encoded bytes across both currencies.
//! Corpus 0.9.13 run: 42218 sections, 0 mismatches everywhere, parse 2.4x.
//! Usage: probe_lite <dim-dir> [rounds]
use c_saves::chunknbt;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

// ---- typed borrowed section shapes (probe copies; to be promoted) ----

#[derive(Debug)]
enum PalLit<'a> {
    Plain(Cow<'a, str>),
    Named {
        name: Cow<'a, str>,
        props: Option<BTreeMap<Cow<'a, str>, Cow<'a, str>>>,
    },
}

#[derive(Deserialize)]
struct PalData<'a> {
    #[serde(borrow)]
    palette: Vec<PalLit<'a>>,
    #[serde(default)]
    data: Option<fastnbt::LongArray>,
}

#[derive(Debug)]
struct PropsMap<'a>(BTreeMap<Cow<'a, str>, Cow<'a, str>>);

impl<'de: 'a, 'a> Deserialize<'de> for PropsMap<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> serde::de::Visitor<'de> for V<'a> {
            type Value = PropsMap<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("string->string properties map")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut m = BTreeMap::new();
                while let Some((k, v)) = a.next_entry::<Cow<'de, str>, Cow<'de, str>>()? {
                    m.insert(k, v);
                }
                Ok(PropsMap(m))
            }
        }
        d.deserialize_map(V(std::marker::PhantomData))
    }
}

/// Root wrapper: the sections list rides under the "" key of the wrap
/// compound we build for fastnbt (NBT roots must be compounds).
#[derive(Deserialize)]
struct RootList<'a> {
    #[serde(rename = "", borrow)]
    list: Vec<SecLite<'a>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecLite<'a> {
    #[serde(rename = "Y")]
    y: i8,
    #[serde(borrow)]
    block_states: Option<PalData<'a>>,
    #[serde(borrow)]
    biomes: Option<PalData<'a>>,
    // derived fields: borrow-and-drop (IgnoredAny can't swallow fastnbt
    // arrays; a borrowed slice keeps the zero-copy parse intact)
    #[serde(rename = "SkyLight", borrow)]
    skylight: Option<&'a [u8]>,
    #[serde(rename = "BlockLight", borrow)]
    blocklight: Option<&'a [u8]>,
    #[serde(rename = "starlight.skylight_state")]
    sl_state: Option<i32>,
    #[serde(rename = "starlight.blocklight_state")]
    bl_state: Option<i32>,
}

impl<'de: 'a, 'a> Deserialize<'de> for PalLit<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> serde::de::Visitor<'de> for V<'a> {
            type Value = PalLit<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("palette entry (string or {Name, Properties})")
            }
            fn visit_borrowed_str<E: serde::de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(PalLit::Plain(Cow::Borrowed(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(PalLit::Plain(Cow::Owned(v.to_string())))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut name: Option<Cow<'a, str>> = None;
                let mut props: Option<BTreeMap<Cow<'a, str>, Cow<'a, str>>> = None;
                while let Some(k) = a.next_key::<Cow<'de, str>>()? {
                    match k.as_ref() {
                        "Name" => {
                            let v: Cow<'de, str> = a.next_value()?;
                            name = Some(v);
                        }
                        "Properties" => {
                            let m: PropsMap = a.next_value()?;
                            props = Some(m.0);
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = a.next_value()?;
                        }
                    }
                }
                let name = name.ok_or_else(|| serde::de::Error::missing_field("Name"))?;
                Ok(PalLit::Named { name, props })
            }
        }
        d.deserialize_any(V(std::marker::PhantomData))
    }
}

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: probe_lite <dim-dir> [rounds]"));
    let rounds: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);

    // collect payloads
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    for z in 0..=31 {
        let f = dim.join("region").join(format!("r.0.{}.mca", z));
        let Ok(data) = std::fs::read(&f) else { continue };
        for c in 0..1024 {
            let off = (c * 4) as usize;
            let oo = (u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) >> 8) as usize * 4096;
            if oo == 0 || oo + 5 > data.len() { continue; }
            let l = u32::from_be_bytes([data[oo], data[oo + 1], data[oo + 2], data[oo + 3]]) as usize;
            if l < 2 || oo + 4 + l > data.len() { continue; }
            let cb = data[oo + 4];
            let raw = &data[oo + 5..oo + 4 + l];
            let payload: Vec<u8> = match cb {
                2 => flate2::read::ZlibDecoder::new(raw).bytes().collect::<Result<Vec<u8>, _>>().unwrap_or_default(),
                1 => flate2::read::GzDecoder::new(raw).bytes().collect::<Result<Vec<u8>, _>>().unwrap_or_default(),
                _ => raw.to_vec(),
            };
            if !payload.is_empty() {
                payloads.push(payload);
            }
        }
    }
    eprintln!("payloads: {}", payloads.len());

    // build wrap bufs (sections list as a named compound root)
    let mut wraps: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // (wrap buf, raw list bytes)
    let mut n_list_elems_total = 0usize;
    for p in &payloads {
        let Some((sp0, sp1)) = chunknbt::scan_sections_range(p) else { continue };
        let mut buf = Vec::with_capacity(6 + (sp1 - sp0));
        buf.extend_from_slice(&[0x0A, 0x00, 0x00, 0x09, 0x00, 0x00]);
        buf.extend_from_slice(&p[sp0..sp1]);
        buf.push(0x00);
        n_list_elems_total += 1;
        wraps.push((buf, p[sp0..sp1].to_vec()));
    }
    eprintln!("wrapped: {}", wraps.len());

    // correctness: Value vs SecLite semantic equality + hash equality proto
    let mut n_secs = 0usize;
    let mut mism = 0usize;
    let mut lite_fail = 0usize;
    let mut hash_mism = 0usize;
    let mut enc_mism = 0usize;
    let mut enc_none_v = 0usize;
    let mut enc_none_l = 0usize;
    let mut enc_none = 0usize;
    let mut hash_mism = 0usize;
    let mut enc_mism = 0usize;
    let mut enc_none_v = 0usize;
    let mut enc_none_l = 0usize;
    for (buf, raw) in &wraps {
        let Ok(fastnbt::Value::Compound(mut m)) = fastnbt::from_bytes::<fastnbt::Value>(buf) else { lite_fail += 1; continue };
        let Some(fastnbt::Value::List(list)) = m.remove("") else { lite_fail += 1; continue };
        let litelist = match fastnbt::from_bytes::<RootList>(buf) {
            Err(e) => {
                lite_fail += 1;
                if lite_fail <= 5 {
                    eprintln!("LITE_FAIL: {e}");
                    for (i, sv) in list.iter().enumerate() {
                        if let fastnbt::Value::Compound(sm) = sv {
                            let mut keys: Vec<&String> = sm.keys().collect();
                            keys.sort();
                            let sig: Vec<String> = keys.iter().map(|k| {
                                let t = match sm[k.as_str()] {
                                    fastnbt::Value::Byte(_) => "b",
                                    fastnbt::Value::Short(_) => "s",
                                    fastnbt::Value::Int(_) => "i",
                                    fastnbt::Value::Long(_) => "l",
                                    fastnbt::Value::Float(_) => "f",
                                    fastnbt::Value::Double(_) => "d",
                                    fastnbt::Value::ByteArray(_) => "B",
                                    fastnbt::Value::String(_) => "S",
                                    fastnbt::Value::IntArray(_) => "I",
                                    fastnbt::Value::LongArray(_) => "L",
                                    fastnbt::Value::List(_) => "T",
                                    fastnbt::Value::Compound(_) => "C",
                                };
                                format!("{k}:{t}")
                            }).collect();
                            eprintln!("  sec[{i}] {}", sig.join(" "));
                        }
                        if i > 6 { break; }
                    }
                }
                continue;
            }
            Ok(RootList { list }) => list,
        };
        if list.len() != litelist.len() { mism += 1; continue; }
        let Some(parity) = chunknbt::lite_parity_probe(raw, true) else {
            lite_fail += 1;
            continue;
        };
        for (hv, hl, ev, el) in &parity {
            if hv != hl {
                hash_mism += 1;
                if hash_mism < 4 {
                    eprintln!("HASH_MISMATCH val={hv:032x} lite={hl:032x}");
                }
            }
            match (ev, el) {
                (None, None) => enc_none += 1,
                (Some(a), Some(b)) if a == b => {}
                (Some(_), None) => enc_none_v += 1,
                (None, Some(_)) => enc_none_l += 1,
                (Some(a), Some(b)) => {
                    enc_mism += 1;
                    if enc_mism < 4 {
                        eprintln!("ENC_MISMATCH vlen={} llen={} first_diff={}", a.len(), b.len(),
                            a.iter().zip(b).position(|(x, y)| x != y).map(|i| i.to_string()).unwrap_or("-".into()));
                    }
                }
            }
        }
        for (sv, sl) in list.iter().zip(litelist.iter()) {
            n_secs += 1;
            if !sec_equal(sv, sl) {
                mism += 1;
                if mism < 4 {
                    eprintln!("MISMATCH: value={:?}\n  lite={:?}", sec_sig(sv), sl_sig(sl));
                }
            }
        }
    }
    eprintln!(
        "equality: sections={n_secs} mismatches={mism} lite_parse_fail_chunks={lite_fail}"
    );
    eprintln!(
        "parity: hash_mismatches={hash_mism} enc_mismatches={enc_mism} enc_none_value={enc_none_v} enc_none_lite={enc_none_l} enc_none_both={enc_none}"
    );

    // timing: A = Value parse (current), B = typed parse
    for r in 0..rounds {
        let t0 = Instant::now();
        let mut nb = 0usize;
        for (buf, _) in &wraps {
            if let Ok(fastnbt::Value::Compound(m)) = fastnbt::from_bytes::<fastnbt::Value>(buf) {
                if let Some(fastnbt::Value::List(l)) = m.get("") {
                    nb += l.len();
                }
            }
        }
        let ta = t0.elapsed();
        let t1 = Instant::now();
        let mut nb2 = 0usize;
        for (buf, _) in &wraps {
            if let Ok(RootList { list }) = fastnbt::from_bytes::<RootList>(buf) {
                nb2 += list.len();
            }
        }
        let tb = t1.elapsed();
        eprintln!(
            "round {r}: value_parse={:?} ({nb} secs)  lite_parse={:?} ({nb2} secs)  speedup={:.2}x",
            ta,
            tb,
            ta.as_nanos() as f64 / tb.as_nanos().max(1) as f64
        );
    }
    let _ = n_list_elems_total;
}

// ---- semantic comparison helpers ----

fn sec_sig(v: &fastnbt::Value) -> String {
    let fastnbt::Value::Compound(m) = v else { return "?".into() };
    let y = if let fastnbt::Value::Byte(b) = m.get("Y").unwrap_or(&fastnbt::Value::Byte(0)) { *b } else { -1 };
    let mut s = format!("Y={y} ");
    if let Some(fastnbt::Value::Compound(bs)) = m.get("block_states") {
        s.push_str("bs[");
        if let Some(fastnbt::Value::List(pal)) = bs.get("palette") {
            for p in pal {
                match p {
                    fastnbt::Value::String(x) => s.push_str(&format!("'{x}' ")),
                    fastnbt::Value::Compound(cm) => {
                        let n = if let Some(fastnbt::Value::String(x)) = cm.get("Name") { x.as_str() } else { "?" };
                        s.push_str(&format!("{{'{n}' props["));
                        if let Some(fastnbt::Value::Compound(pr)) = cm.get("Properties") {
                            let mut kv: Vec<(&String, &String)> = Vec::new();
                            for (k, v) in pr {
                                if let fastnbt::Value::String(vv) = v { kv.push((k, vv)); }
                            }
                            kv.sort();
                            for (k, v) in kv { s.push_str(&format!("{k}={v} ")); }
                        }
                        s.push_str("]} ");
                    }
                    _ => s.push_str("? "),
                }
            }
        }
        if let Some(fastnbt::Value::LongArray(d)) = bs.get("data") {
            s.push_str(&format!("data_len={}", d.len()));
        }
        s.push_str("] ");
    }
    if let Some(fastnbt::Value::Compound(bs)) = m.get("biomes") {
        s.push_str("bio[");
        if let Some(fastnbt::Value::List(pal)) = bs.get("palette") {
            s.push_str(&format!("n={} ", pal.len()));
        }
        if let Some(fastnbt::Value::LongArray(d)) = bs.get("data") {
            s.push_str(&format!("data_len={}", d.len()));
        }
        s.push_str("]");
    }
    s
}

fn sl_sig<'a>(s: &SecLite<'a>) -> String {
    let mut out = format!("Y={} ", s.y);
    if let Some(bs) = &s.block_states {
        out.push_str("bs[");
        for p in &bs.palette {
            match p {
                PalLit::Plain(x) => out.push_str(&format!("'{x}' ")),
                PalLit::Named { name, props } => {
                    out.push_str(&format!("{{'{name}' props["));
                    if let Some(m) = props {
                        for (k, v) in m { out.push_str(&format!("{k}={v} ")); }
                    }
                    out.push_str("]} ");
                }
            }
        }
        if let Some(d) = &bs.data { out.push_str(&format!("data_len={}", d.len())); }
        out.push_str("] ");
    }
    if let Some(bs) = &s.biomes {
        out.push_str(&format!("bio[n={} ", bs.palette.len()));
        if let Some(d) = &bs.data { out.push_str(&format!("data_len={}", d.len())); }
        out.push_str("]");
    }
    out
}

fn sec_equal(v: &fastnbt::Value, s: &SecLite) -> bool {
    let fastnbt::Value::Compound(m) = v else { return false };
    match m.get("Y") {
        Some(fastnbt::Value::Byte(b)) if *b == s.y => {}
        _ => return false,
    }
    // block_states
    let bs_v = m.get("block_states");
    match (bs_v, &s.block_states) {
        (None, None) => {}
        (Some(fastnbt::Value::Compound(bv)), Some(sl)) => {
            if !pal_data_equal(bv, &sl.palette, sl.data.as_deref()) { return false; }
        }
        _ => return false,
    }
    match (m.get("biomes"), &s.biomes) {
        (None, None) => {}
        (Some(fastnbt::Value::Compound(bv)), Some(sl)) => {
            if !pal_data_equal(bv, &sl.palette, sl.data.as_deref()) { return false; }
        }
        _ => return false,
    }
    true
}

fn pal_data_equal(
    bv: &std::collections::HashMap<String, fastnbt::Value>,
    lit: &[PalLit],
    data: Option<&[i64]>,
) -> bool {
    let Some(fastnbt::Value::List(pal)) = bv.get("palette") else { return false };
    if pal.len() != lit.len() { return false; }
    for (pv, pl) in pal.iter().zip(lit) {
        match (pv, pl) {
            (fastnbt::Value::String(x), PalLit::Plain(y)) if x == y => {}
            (fastnbt::Value::Compound(cm), PalLit::Named { name, props }) => {
                match cm.get("Name") {
                    Some(fastnbt::Value::String(x)) if x == name => {}
                    _ => return false,
                }
                match (cm.get("Properties"), props) {
                    (None, None) => {}
                    (Some(fastnbt::Value::Compound(pr)), Some(m)) => {
                        if pr.len() != m.len() { return false; }
                        for (k, v) in pr {
                            match (v, m.get(k.as_str())) {
                                (fastnbt::Value::String(vv), Some(mv)) if vv == mv => {}
                                _ => return false,
                            }
                        }
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    match (bv.get("data"), data) {
        (None, None) => {}
        (Some(fastnbt::Value::LongArray(a)), Some(b)) if a.as_ref() == b => {}
        _ => return false,
    }
    true
}
