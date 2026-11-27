//! cm_probe: the pool-wide CM arithmetic coder on the real g26 grid class —
//! the one encoder family never tried. LZMA cannot see the vertical axis
//! (vanilla order strides 256 voxels between above-neighbors) and the
//! 0.9.119 lc/lp nibble-phase ladder was flat because it still only
//! rearranges LZMA's match machinery. A context-modeling range coder
//! MODELS strata: ctx = (voxel_above, y_plane) makes underground runs
//! near-deterministic and prices only the true per-column jitter.
//!
//! Variants (grids only — the 6-B headers ride LZMA today and stay):
//!   A  LZMA2 p6e lc4lp0pb0 over the concatenated vanilla-order grids (fact)
//!   B  the same LZMA2 over column-major-transposed grids (the missing
//!      dense-transposed admission form)
//!   C  CM v1: ctx = (above_sym, plane), pool-wide, no per-member reset
//!   D  CM v2: ctx = (above_sym, left_sym, plane>>1)
//!   E  CM v1 over the transposed order (vertical neighbor = adjacent nibble)
//! The winner config is roundtripped byte-exact (decode == input).
//!
//! Usage: cargo run --release --example cm_probe [store-dir]

use c_saves::store::Store;
use std::path::PathBuf;

const TOP: u32 = 1 << 24;

// ---------------- the range coder (7-zip style, carry-delayed) -------------

struct RangeEnc {
    low: u64,
    range: u32,
    cache_size: u64,
    cache: u8,
    out: Vec<u8>,
}

impl RangeEnc {
    fn new() -> Self {
        Self { low: 0, range: u32::MAX, cache_size: 1, cache: 0, out: Vec::new() }
    }
    #[inline]
    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = (self.low >> 24) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low << 8) & 0xFFFF_FFFF;
    }
    /// encode one symbol with cumulative freq `cum` of `freq` out of `tot`
    #[inline]
    fn enc(&mut self, cum: u32, freq: u32, tot: u32) {
        let r = self.range / tot;
        self.low += (cum as u64) * (r as u64);
        self.range = r * freq;
        while self.range < TOP {
            self.shift_low();
            self.range <<= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

struct RangeDec<'a> {
    code: u32,
    range: u32,
    src: &'a [u8],
    pos: usize,
}

impl<'a> RangeDec<'a> {
    fn new(src: &'a [u8]) -> Self {
        // first byte is the always-zero opener, next 4 are the code
        let code = u32::from_be_bytes([src[1], src[2], src[3], src[4]]);
        Self { code, range: u32::MAX, src, pos: 5 }
    }
    #[inline]
    fn norm(&mut self) {
        while self.range < TOP {
            self.code = (self.code << 8) | (self.src[self.pos] as u32);
            self.pos += 1;
            self.range <<= 8;
        }
    }
    /// decode one symbol given the freq slice; returns (symbol, cum, freq, tot)
    #[inline]
    fn dec(&mut self, counts: &[u32], tot: u32) -> (usize, u32, u32, u32) {
        let r = self.range / tot;
        let target = self.code / r;
        let mut cum = 0u32;
        let mut s = 0usize;
        while s + 1 < counts.len() {
            let f = counts[s];
            if cum + f > target {
                break;
            }
            cum += f;
            s += 1;
        }
        let freq = counts[s];
        self.code -= cum * r;
        self.range = freq * r;
        self.norm();
        (s, cum, freq, tot)
    }
}

// ---------------- adaptive models -------------------------------------------

struct ModelB {
    // ctx -> counts over `nsym` symbols (the rest of each row stays zero)
    counts: Vec<[u32; 256]>,
    tot: Vec<u32>,
    nsym: usize,
    inc: u32,
    limit: u32,
}

impl ModelB {
    fn new(nctx: usize, nsym: usize, inc: u32, limit: u32) -> Self {
        let mut counts = vec![[0u32; 256]; nctx];
        for row in &mut counts {
            for c in row.iter_mut().take(nsym) {
                *c = 1;
            }
        }
        Self { counts, tot: vec![nsym as u32; nctx], nsym, inc, limit }
    }
    #[inline]
    fn update(&mut self, ctx: usize, s: usize) {
        let row = &mut self.counts[ctx];
        row[s] += self.inc;
        let t = &mut self.tot[ctx];
        *t += self.inc;
        if *t > self.limit {
            let mut nt = 0u32;
            for x in row.iter_mut().take(self.nsym) {
                *x = (*x >> 1).max(1);
                nt += *x;
            }
            *t = nt;
        }
    }
}

/// byte-mode CM v1: ctx = prev*16 + (plane>>1) — the previous symbol plus a
/// coarse depth band (prev is the strong predictor in the transposed order:
/// consecutive bytes run down one column)
fn cm_encode_u8(grids: &[Vec<u8>], inc: u32, limit: u32, side_ctx: bool) -> Vec<u8> {
    let nctx = if side_ctx { 256 * 32 * 4 } else { 256 * 16 };
    let mut m = ModelB::new(nctx, 256, inc, limit);
    let mut rc = RangeEnc::new();
    for g in grids {
        for (i, &s) in g.iter().enumerate() {
            let plane = (i >> 8) as usize;
            let prev = if i > 0 { g[i - 1] as usize } else { 0 };
            let ctx = if side_ctx {
                let side = if i >= 16 { (g[i - 16] >> 3) as usize } else { 31 };
                prev * 128 + side * 4 + (plane >> 2)
            } else {
                prev * 16 + (plane >> 1)
            };
            let counts = m.counts[ctx];
            let tot = m.tot[ctx];
            let mut cum = 0u32;
            for c in counts.iter().take(s as usize) {
                cum += c;
            }
            rc.enc(cum, counts[s as usize], tot);
            m.update(ctx, s as usize);
        }
    }
    rc.finish()
}

fn cm_decode_u8(grids: &[Vec<u8>], inc: u32, limit: u32, side_ctx: bool, src: &[u8]) -> Vec<u8> {
    let nctx = if side_ctx { 256 * 32 * 4 } else { 256 * 16 };
    let mut m = ModelB::new(nctx, 256, inc, limit);
    let mut rd = RangeDec::new(src);
    let mut out = Vec::with_capacity(grids.iter().map(|g| g.len()).sum());
    for g in grids {
        for i in 0..g.len() {
            let plane = (i >> 8) as usize;
            let prev = if i > 0 { out[out.len() - 1] as usize } else { 0 };
            let ctx = if side_ctx {
                let side = if i >= 16 { (out[out.len() - 16] >> 3) as usize } else { 31 };
                prev * 128 + side * 4 + (plane >> 2)
            } else {
                prev * 16 + (plane >> 1)
            };
            let counts = m.counts[ctx];
            let tot = m.tot[ctx];
            let (s, cum, freq, _) = rd.dec(&counts, tot);
            out.push(s as u8);
            debug_assert_eq!(freq, counts[s]);
            debug_assert_eq!(cum, counts[..s].iter().sum::<u32>());
            m.update(ctx, s);
        }
    }
    out
}

// ---------------- adaptive nibble model --------------------------------------

const NSYM: usize = 16;

struct Model {
    // ctx -> [16] counts
    counts: Vec<[u32; NSYM]>,
    tot: Vec<u32>,
    inc: u32,
    limit: u32,
}

impl Model {
    fn new(nctx: usize, inc: u32, limit: u32) -> Self {
        Self { counts: vec![[1; NSYM]; nctx], tot: vec![NSYM as u32; nctx], inc, limit }
    }
    #[inline]
    fn update(&mut self, ctx: usize, s: usize) {
        let c = &mut self.counts[ctx];
        c[s] += self.inc;
        let t = &mut self.tot[ctx];
        *t += self.inc;
        if *t > self.limit {
            let mut nt = 0u32;
            for x in c.iter_mut() {
                *x = (*x >> 1).max(1);
                nt += *x;
            }
            *t = nt;
        }
    }
}

// ---------------- grid extraction -------------------------------------------

fn nibbles(grid: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(grid.len() * 2);
    for &b in grid {
        v.push(b >> 4);
        v.push(b & 15);
    }
    v
}

fn pack(v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() / 2);
    for p in v.chunks(2) {
        out.push((p[0] << 4) | p[1]);
    }
    out
}

/// vanilla i = y*256 + z*16 + x  ->  column-major t = (x*16 + z)*16 + y
fn transpose(v: &[u8]) -> Vec<u8> {
    let mut t = vec![0u8; v.len()];
    for y in 0..16usize {
        for z in 0..16usize {
            for x in 0..16usize {
                t[(x * 16 + z) * 16 + y] = v[y * 256 + z * 16 + x];
            }
        }
    }
    t
}

fn untranspose(t: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; t.len()];
    for y in 0..16usize {
        for z in 0..16usize {
            for x in 0..16usize {
                v[y * 256 + z * 16 + x] = t[(x * 16 + z) * 16 + y];
            }
        }
    }
    v
}

// ---------------- CM encoders -----------------------------------------------

/// CM v1: ctx = plane*17 + above (above = 16 sentinel for the top plane);
/// geometry is per-grid (local indices), the model is pool-wide (no reset)
fn cm_encode_v1(grids: &[Vec<u8>], inc: u32, limit: u32) -> Vec<u8> {
    let mut m = Model::new(16 * 17, inc, limit);
    let mut rc = RangeEnc::new();
    for g in grids {
        for (i, &s) in g.iter().enumerate() {
            let plane = (i >> 8) as usize;
            let above_idx = if i >= 256 { i - 256 } else { usize::MAX };
            let av = if above_idx == usize::MAX { 16 } else { g[above_idx] as usize };
            let ctx = plane * 17 + av;
            let counts = m.counts[ctx];
            let tot = m.tot[ctx];
            let mut cum = 0u32;
            for c in counts.iter().take(s as usize) {
                cum += c;
            }
            rc.enc(cum, counts[s as usize], tot);
            m.update(ctx, s as usize);
        }
    }
    rc.finish()
}

fn cm_decode_v1(grids: &[Vec<u8>], inc: u32, limit: u32, src: &[u8]) -> Vec<u8> {
    let mut m = Model::new(16 * 17, inc, limit);
    let mut rd = RangeDec::new(src);
    let mut out = Vec::with_capacity(grids.iter().map(|g| g.len()).sum());
    for g in grids {
        for i in 0..g.len() {
            let plane = (i >> 8) as usize;
            // the grid is 4096 nibbles: at global position out.len() the local
            // index is i, so the above-neighbor is exactly 256 back
            let av = if i >= 256 { out[out.len() - 256] as usize } else { 16 };
            let ctx = plane * 17 + av;
            let counts = m.counts[ctx];
            let tot = m.tot[ctx];
            let (s, cum, freq, _) = rd.dec(&counts, tot);
            out.push(s as u8);
            let mut cc = 0u32;
            for c in counts.iter().take(s) {
                cc += c;
            }
            debug_assert_eq!(cc, cum);
            debug_assert_eq!(freq, counts[s]);
            m.update(ctx, s);
        }
    }
    out
}

/// CM v2: ctx = (above*17 + left)*8 + (plane>>1); left = 16 sentinel at x==0
fn cm_encode_v2(grids: &[Vec<u8>], inc: u32, limit: u32) -> Vec<u8> {
    let mut m = Model::new(17 * 17 * 8, inc, limit);
    let mut rc = RangeEnc::new();
    for g in grids {
        for i in 0..g.len() {
            let plane = ((i >> 8) >> 1) as usize;
            let above_idx = if i >= 256 { i - 256 } else { usize::MAX };
            let av = if above_idx == usize::MAX { 16 } else { g[above_idx] as usize };
            let lv = if (i & 15) == 0 { 16 } else { g[i - 1] as usize };
            let ctx = (av * 17 + lv) * 8 + plane;
            let counts = m.counts[ctx];
            let tot = m.tot[ctx];
            let s = g[i] as usize;
            let mut cum = 0u32;
            for c in counts.iter().take(s) {
                cum += c;
            }
            rc.enc(cum, counts[s], tot);
            m.update(ctx, s);
        }
    }
    rc.finish()
}

fn cm_decode_v2(grids: &[Vec<u8>], inc: u32, limit: u32, src: &[u8]) -> Vec<u8> {
    let mut m = Model::new(17 * 17 * 8, inc, limit);
    let mut rd = RangeDec::new(src);
    let mut out = Vec::with_capacity(grids.iter().map(|g| g.len()).sum());
    for g in grids {
        for i in 0..g.len() {
            let plane = ((i >> 8) >> 1) as usize;
            let av = if i >= 256 { out[out.len() - 256] as usize } else { 16 };
            let lv = if (i & 15) == 0 { 16 } else { out[out.len() - 1] as usize };
            let ctx = (av * 17 + lv) * 8 + plane;
            let counts = m.counts[ctx];
            let tot = m.tot[ctx];
            let (s, _, _, _) = rd.dec(&counts, tot);
            out.push(s as u8);
            m.update(ctx, s);
        }
    }
    out
}

// ---------------- LZMA2 helper (the shipped tuned config) --------------------

fn lzma2_p6e(data: &[u8]) -> Vec<u8> {
    use liblzma::stream as lz;
    let mut opts = lz::LzmaOptions::new_preset(6 | lz::PRESET_EXTREME).unwrap();
    opts.literal_context_bits(4).literal_position_bits(0).position_bits(0);
    let mut f = lz::Filters::new();
    f.lzma2(&opts);
    let mut s = lz::Stream::new_raw_encoder(&f).unwrap();
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    let mut consumed = 0usize;
    while consumed < data.len() {
        if out.len() == out.capacity() {
            out.reserve(64 * 1024);
        }
        let before = s.total_in() as usize;
        s.process_vec(&data[consumed..], &mut out, lz::Action::Run).unwrap();
        let now = s.total_in() as usize;
        if now == before {
            panic!("lzma no progress");
        }
        consumed = now;
    }
    loop {
        if out.len() == out.capacity() {
            out.reserve(64 * 1024);
        }
        let st = s.process_vec(&[], &mut out, lz::Action::Finish).unwrap();
        if matches!(st, lz::Status::StreamEnd) {
            break;
        }
    }
    out
}

fn main() {
    let dir = PathBuf::from(
        std::env::args().nth(1).unwrap_or_else(|| "/tmp/full_world/store-overworld".to_string()),
    );
    let class = std::env::args().nth(2).unwrap_or_else(|| "glob_g26".to_string());
    let byte_mode = class == "glob_g6"; // transposed u8 gid grids (mountains)
    let st = Store::open(&dir, true).expect("store opens");
    let members = st.slab_members_by_class();
    let mut grids: Vec<&[u8]> = Vec::new();
    let mut n = 0usize;
    let mut sizes: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (cls, body) in &members {
        if *cls != class {
            continue;
        }
        n += 1;
        *sizes.entry(body.len()).or_default() += 1;
        let grid_len = if byte_mode { 4096 } else { 2048 };
        if body.len() >= grid_len {
            grids.push(&body[body.len() - grid_len..]);
        }
    }
    println!("{class} members={n} distinct-body-sizes={}", sizes.len());
    let mut ds: Vec<_> = sizes.iter().collect();
    ds.sort();
    for (sz, c) in ds.iter().take(6) {
        println!("  size {sz}: {c} members");
    }
    if grids.is_empty() {
        println!("no g26 grids — nothing to price");
        return;
    }
    let raw_grid: usize = grids.iter().map(|g| g.len()).sum();

    if byte_mode {
        // A: LZMA over the stored (transposed) grids — the shipped form
        let mut a_stream = Vec::with_capacity(raw_grid);
        for g in &grids {
            a_stream.extend_from_slice(g);
        }
        let t0 = std::time::Instant::now();
        let a = lzma2_p6e(&a_stream);
        let ta = t0.elapsed();
        // A2: LZMA over the un-transposed (vanilla-order) grids
        let mut a2_stream = Vec::with_capacity(raw_grid);
        for g in &grids {
            a2_stream.extend_from_slice(&untranspose(g));
        }
        let a2 = lzma2_p6e(&a2_stream);
        // CM byte-mode variants (pool-wide, no per-member reset)
        let bg: Vec<Vec<u8>> = grids.iter().map(|g| g.to_vec()).collect();
        let t0 = std::time::Instant::now();
        let c = cm_encode_u8(&bg, 8, 4096, false);
        let tc = t0.elapsed();
        let d = cm_encode_u8(&bg, 8, 4096, true);
        println!(
            "g6 grids {raw_grid} B | A lzma-as-is {} ({ta:.1?}) | A2 lzma-untransposed {} | C cm(prev,plane) {} ({tc:.1?}) | D cm(prev,side,plane) {}",
            a.len(),
            a2.len(),
            c.len(),
            d.len()
        );
        println!(
            "per-member: A {:.1} | A2 {:.1} | C {:.1} | D {:.1}",
            a.len() as f64 / grids.len() as f64,
            a2.len() as f64 / grids.len() as f64,
            c.len() as f64 / grids.len() as f64,
            d.len() as f64 / grids.len() as f64
        );
        let back = cm_decode_u8(&bg, 8, 4096, false, &c);
        assert_eq!(back, bg.concat(), "cm-u8 v1 roundtrip broken");
        let back2 = cm_decode_u8(&bg, 8, 4096, true, &d);
        assert_eq!(back2, bg.concat(), "cm-u8 side roundtrip broken");
        println!("  roundtrips: C/D byte-exact OK");
        return;
    }

    // A: LZMA over vanilla-order grids (the shipped dense form)
    let mut a_stream = Vec::with_capacity(raw_grid);
    for g in &grids {
        a_stream.extend_from_slice(g);
    }
    let t0 = std::time::Instant::now();
    let a = lzma2_p6e(&a_stream);
    let ta = t0.elapsed();

    // B: LZMA over column-major-transposed grids
    let mut b_stream = Vec::with_capacity(raw_grid);
    for g in &grids {
        b_stream.extend_from_slice(&transpose(&nibbles(g)));
    }
    let t0 = std::time::Instant::now();
    let b = lzma2_p6e(&b_stream);
    let tb = t0.elapsed();

    // the CM streams: per-grid nibbles, pool-wide model, no per-member reset
    let nib_grids: Vec<Vec<u8>> = grids.iter().map(|g| nibbles(g)).collect();
    let tn_grids: Vec<Vec<u8>> = nib_grids.iter().map(|g| transpose(g)).collect();
    // C/D: CM variants with two adaptation configs each
    for (inc, limit) in [(8u32, 4096u32), (16, 8192)] {
        let t0 = std::time::Instant::now();
        let c = cm_encode_v1(&nib_grids, inc, limit);
        let tc = t0.elapsed();
        let d = cm_encode_v2(&nib_grids, inc, limit);
        // E: CM v1 over the transposed nibble order
        let e = cm_encode_v1(&tn_grids, inc, limit);
        println!(
            "inc={inc} limit={limit}: grids {raw_grid} B | A lzma-vanilla {} ({ta:.1?}) | B lzma-transposed {} ({tb:.1?}) | C cm-v1 {} ({tc:.1?}) | D cm-v2 {} | E cm-v1-transposed {}",
            a.len(),
            b.len(),
            c.len(),
            d.len(),
            e.len()
        );
        if inc == 8 {
            // roundtrip the two CM decoders byte-exact
            let back = cm_decode_v1(&nib_grids, inc, limit, &c);
            assert_eq!(back, nib_grids.concat(), "cm v1 roundtrip broken");
            let back2 = cm_decode_v2(&nib_grids, inc, limit, &d);
            assert_eq!(back2, nib_grids.concat(), "cm v2 roundtrip broken");
            let back3 = cm_decode_v1(&tn_grids, inc, limit, &e);
            assert_eq!(back3, tn_grids.concat(), "cm v1 transposed roundtrip broken");
            // transpose is lossless by construction
            let g0 = nibbles(grids[0]);
            assert_eq!(untranspose(&transpose(&g0)), g0);
            println!("  roundtrips: C/D/E byte-exact OK + transpose lossless");
        }
    }
    println!(
        "per-member: A {:.1} B | B {:.1} | C(best) {:.1}",
        a.len() as f64 / grids.len() as f64,
        b.len() as f64 / grids.len() as f64,
        cm_encode_v1(&nib_grids, 8, 4096).len() as f64 / grids.len() as f64
    );
}
