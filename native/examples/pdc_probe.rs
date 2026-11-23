//! pdc_probe: price the preset-dict chained-parts (PDC) encode axis.
//!
//! The pack wall's pole is the ONE big raw LZMA2 stream (the narrow glob
//! twin): stream-level parallelism saturates (cores.min(streams)) but a
//! single 10 MiB stream pins one core for ~6 s at preset 6|E. The blocked
//! .xz MT encoder parallelizes the SAME stream but resets the LZMA2
//! context at every block boundary (measured +5.1% bytes at 2 blocks,
//! v0.9.120). PDC chains the parts through lzma_options_lzma.preset_dict:
//! part i encodes with preset_dict = the full preceding raw stream (the
//! preset-6 8 MiB dict window covers everything the single-context
//! encoder could reference), so the only divergence is the probability-
//! model reset — the hypothesis is a near-zero byte delta at full
//! part-level parallelism.
//!
//! Method (real dumped members, the lex-ordered pack window): baseline =
//! tuned p6e single context; PDC K in {2,3,4,6,8} with parts split at raw
//! byte ranges, part i>0 carrying preset_dict = raw[..start_i] clipped to
//! 8 MiB; every config DECODES back through the same chain (part i's
//! decoder preset_dict = the decoded tail of part i-1, clip 8 MiB) and
//! asserts byte equality. Walls: encode measured BOTH sequentially (work)
//! and with K parallel threads (the makespan the pack would see). HC4
//! match-finder rows price the cheaper-model alternative for reference.
//!
//! Usage: pdc_probe <dump.bin> [K...]
use liblzma_sys as sys;
use std::time::Instant;

const DICT_MAX: usize = 8 * 1024 * 1024; // preset-6 dict size

/// tuned preset 6|EXTREME lc4/lp0/pb0 (+ optional preset dict) via the raw
/// C struct — the same transmute lzma_dict_probe proved layout-identical.
fn tuned_opts(dict: Option<&[u8]>, hc4: bool) -> liblzma::stream::LzmaOptions {
    unsafe {
        let mut raw: sys::lzma_options_lzma = std::mem::zeroed();
        assert_eq!(sys::lzma_lzma_preset(&mut raw, 6 | sys::LZMA_PRESET_EXTREME), 0u8);
        raw.lc = 4;
        raw.lp = 0;
        raw.pb = 0;
        if hc4 {
            raw.dict_size = DICT_MAX as u32;
            raw.nice_len = 273;
            raw.mf = sys::LZMA_MF_HC4;
            raw.depth = 0;
        }
        if let Some(d) = dict {
            raw.preset_dict = d.as_ptr();
            raw.preset_dict_size = d.len() as u32;
        }
        std::mem::transmute::<sys::lzma_options_lzma, liblzma::stream::LzmaOptions>(raw)
    }
}

fn lzma2_enc(data: &[u8], dict: Option<&[u8]>, hc4: bool) -> Vec<u8> {
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&tuned_opts(dict, hc4));
    let mut s = liblzma::stream::Stream::new_raw_encoder(&f).expect("enc stream");
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    s.process_vec(data, &mut out, liblzma::stream::Action::Run).expect("enc run");
    s.process_vec(&[], &mut out, liblzma::stream::Action::Finish).expect("enc fin");
    out
}

fn lzma2_dec(data: &[u8], raw_len: usize, dict: Option<&[u8]>) -> Vec<u8> {
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&tuned_opts(dict, false));
    let mut s = liblzma::stream::Stream::new_raw_decoder(&f).expect("dec stream");
    let mut out = Vec::with_capacity(raw_len);
    s.process_vec(data, &mut out, liblzma::stream::Action::Run).expect("dec run");
    out
}

/// one PDC config: returns (comp bytes, seq enc wall, par enc wall, dec wall)
fn pdc_run(raw: &[u8], k: usize) -> (u64, f64, f64, f64) {
    // split at raw byte ranges: k equal parts
    let part = raw.len().div_ceil(k);
    let mut bounds = Vec::with_capacity(k + 1);
    let mut b = 0usize;
    for _ in 0..k {
        bounds.push(b);
        b = (b + part).min(raw.len());
    }
    bounds.push(raw.len());
    let bounds = bounds;

    // encode parts (sequential wall = work measure)
    let t0 = Instant::now();
    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(k);
    for i in 0..k {
        let start = bounds[i];
        let dict = if i == 0 || start == 0 {
            None
        } else {
            let from = start.saturating_sub(DICT_MAX);
            Some(&raw[from..start])
        };
        parts.push(lzma2_enc(&raw[bounds[i]..bounds[i + 1]], dict, false));
    }
    let seq = t0.elapsed().as_secs_f64();
    let comp: u64 = parts.iter().map(|p| p.len() as u64).sum();

    // decode back through the chain, asserting equality
    let t0 = Instant::now();
    let mut acc: Vec<u8> = Vec::new();
    for i in 0..k {
        let dict = if acc.is_empty() {
            None
        } else {
            let from = acc.len().saturating_sub(DICT_MAX);
            Some(&acc[from..])
        };
        let got = lzma2_dec(&parts[i], bounds[i + 1] - bounds[i], dict);
        assert_eq!(&got[..], &raw[bounds[i]..bounds[i + 1]], "PDC part {i} roundtrip");
        acc.extend_from_slice(&got);
        let keep = acc.len().saturating_sub(DICT_MAX);
        acc.drain(..keep);
    }
    let dec = t0.elapsed().as_secs_f64();

    let t0 = Instant::now();
    let outs: Vec<std::sync::Mutex<Option<Vec<u8>>>> =
        (0..k).map(|_| std::sync::Mutex::new(None)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|sc| {
        let outs = &outs;
        let next = &next;
        let bounds = &bounds;
        let par = k.min(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
        for _ in 0..par {
            sc.spawn(move || loop {
                let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if j >= k {
                    break;
                }
                let start = bounds[j];
                let dict = if j == 0 || start == 0 {
                    None
                } else {
                    let from = start.saturating_sub(DICT_MAX);
                    Some(&raw[from..start])
                };
                let c = lzma2_enc(&raw[bounds[j]..bounds[j + 1]], dict, false);
                *outs[j].lock().unwrap() = Some(c);
            });
        }
    });
    let par_wall = t0.elapsed().as_secs_f64();

    (comp, seq, par_wall, dec)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: pdc_probe <dump.bin> [K...]");
    let ks: Vec<usize> = std::env::args()
        .skip(2)
        .map(|s| s.parse().expect("K"))
        .collect();
    let ks = if ks.is_empty() { vec![2, 3, 4, 6, 8] } else { ks };
    let raw = std::fs::read(&path).expect("dump");
    println!(
        "== pdc_probe {} = {:.2} MiB, cores {} ==",
        path,
        raw.len() as f64 / (1 << 20) as f64,
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    );

    // baseline: the shipped single-context tuned encoder
    let t0 = Instant::now();
    let base = lzma2_enc(&raw, None, false);
    let base_enc = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    let back = lzma2_dec(&base, raw.len(), None);
    let base_dec = t0.elapsed().as_secs_f64();
    assert_eq!(&back[..], &raw, "baseline roundtrip");
    println!(
        "single tuned p6e:   {:>9} B  enc(work) {:.2}s  dec {:.3}s",
        base.len(),
        base_enc,
        base_dec
    );

    // HC4 reference rows (encode-side model swap, same dict)
    let t0 = Instant::now();
    let hc4 = lzma2_enc(&raw, None, true);
    let hc4_enc = t0.elapsed().as_secs_f64();
    assert_eq!(&lzma2_dec(&hc4, raw.len(), None)[..], &raw, "HC4 roundtrip");
    println!(
        "HC4 nice273:        {:>9} B  ({:+.2}%)  enc(work) {:.2}s",
        hc4.len(),
        (hc4.len() as f64 - base.len() as f64) / base.len() as f64 * 100.0,
        hc4_enc
    );

    for k in ks {
        let (comp, seq, par_wall, dec) = pdc_run(&raw, k);
        println!(
            "PDC K={k:<2}           {:>9} B  ({:+.3}%)  enc(work) {:.2}s  enc(par) {:.2}s  dec {:.3}s",
            comp,
            (comp as f64 - base.len() as f64) / base.len() as f64 * 100.0,
            seq,
            par_wall,
            dec
        );
    }
}
