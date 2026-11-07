//! KT-adaptive context-modeling coder for canonical voxel index streams
//! (v0.9.42, section form 3).
//!
//! Model (matches the offline probes in docs/compression-research.md
//! exactly — the closed-form KT code length measured -11.9% on section
//! bodies vs dense/RLE + LZMA2-6 at W<=4):
//!   - alphabet K = 1 << W symbols (W = bits_for(palette_len) <= 4);
//!   - context c = (left, above): the previous voxel's canonical index and
//!     the voxel 256 positions earlier (y-1 in the y*256+z*16+x section
//!     layout), both 0 at boundaries;
//!   - per-context adaptive counts with the Krichevsky-Trofimov prior:
//!     freq(s) = 2*count(s)+1, total = 2*used + K  ==  (count+0.5)/(used+K/2);
//!   - counts start at zero (cold start), never transmitted — the decoder
//!     rebuilds the identical model evolution symbol by symbol.
//! Coder: LZMA-convention carry range encoder/decoder (32-bit range,
//! 5-byte init/flush pairing). Deterministic pure integer arithmetic —
//! identical sections encode to identical bytes (dedup-stable).

/// Range normalization threshold (LZMA kTopValue).
const TOP: u32 = 1 << 24;

struct RangeEnc {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
}

impl RangeEnc {
    fn new() -> Self {
        Self {
            low: 0,
            range: u32::MAX,
            cache: 0,
            cache_size: 1,
            out: Vec::with_capacity(640),
        }
    }

    // LZMA-convention ShiftLow: emits the pending byte plus a carry run,
    // consumes the carry bit, slides the window one byte up.
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
        // LZMA: Low = (UInt32)Low << 8 — the carry bit was consumed above
        self.low = ((self.low as u32) << 8) as u64;
    }

    #[inline]
    fn encode(&mut self, cum: u32, freq: u32, tot: u32) {
        self.range /= tot;
        self.low += (cum as u64) * (self.range as u64);
        self.range *= freq; // (range/tot)*freq <= range < 2^32: no overflow
        while self.range < TOP {
            self.range <<= 8;
            self.shift_low();
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
    range: u32,
    code: u32,
    b: &'a [u8],
    p: usize,
}

impl<'a> RangeDec<'a> {
    /// LZMA pairing: the first 5 stream bytes fold into a 32-bit code (the
    /// encoder's leading 0x00 shifts out of the top).
    fn new(b: &'a [u8]) -> Option<Self> {
        if b.len() < 5 {
            return None;
        }
        let mut code: u32 = 0;
        for i in 0..5 {
            code = (code << 8) | b[i] as u32;
        }
        Some(Self {
            range: u32::MAX,
            code,
            b,
            p: 5,
        })
    }

    #[inline]
    fn next_byte(&mut self) -> Option<u32> {
        let x = *self.b.get(self.p)? as u32;
        self.p += 1;
        Some(x)
    }

    #[inline]
    fn normalize(&mut self) -> Option<()> {
        while self.range < TOP {
            self.range <<= 8;
            self.code = (self.code << 8) | self.next_byte()?;
        }
        Some(())
    }

    /// Decode one symbol given the per-symbol frequencies (KT freqs,
    /// sum == tot). Linear scan (K <= 16).
    #[inline]
    fn decode_symbol(&mut self, freqs: &[u32], tot: u32) -> Option<usize> {
        self.range /= tot;
        let target = self.code / self.range;
        let mut cum: u32 = 0;
        for (s, &f) in freqs.iter().enumerate() {
            if cum + f > target {
                self.code -= ((cum as u64) * (self.range as u64)) as u32;
                self.range *= f;
                self.normalize()?;
                return Some(s);
            }
            cum += f;
        }
        None
    }
}

/// Encode the canonical index stream: `v` carries vanilla palette indices,
/// `remap` maps them to the canonical (frequency) order — the identical
/// mapping dense_pack_mapped/rle_encode_stream_mapped apply. The context
/// model operates in canonical space (values < K = 1<<w). Returns the raw
/// coder bytes (no framing). None on out-of-range input.
pub fn encode_canon(v: &[u32], remap: &[u8], w: u32) -> Option<Vec<u8>> {
    if w == 0 || w > 4 || v.is_empty() || remap.is_empty() {
        return None;
    }
    let k = 1usize << w;
    let kk = k * k;
    // 256-stride `above` context requires the 4096-voxel layout.
    if v.len() != 4096 {
        return None;
    }
    let mut counts = vec![0u32; kk * k];
    let mut freqs = [0u32; 16];
    let mut rc = RangeEnc::new();
    let mut left = 0usize;
    for (i, &raw) in v.iter().enumerate() {
        let x = *remap.get(raw as usize)? as usize;
        if x >= k {
            return None;
        }
        let above = if i >= 256 {
            *remap.get(v[i - 256] as usize)? as usize
        } else {
            0
        };
        let row = (left * k + above) * k;
        let mut tot: u32 = 0;
        let mut cum: u32 = 0;
        for (j, fj) in freqs.iter_mut().enumerate().take(k) {
            let f = 2 * counts[row + j] + 1;
            *fj = f;
            if j < x {
                cum += f;
            }
            tot += f;
        }
        let freq = freqs[x];
        rc.encode(cum, freq, tot);
        counts[row + x] += 1;
        left = x;
    }
    Some(rc.finish())
}

/// Decode `entries` canonical indices from the raw coder bytes (the exact
/// mirror of encode_canon). None on corrupt input.
pub fn decode_canon(b: &[u8], w: u32, entries: usize) -> Option<Vec<u8>> {
    if w == 0 || w > 4 || entries != 4096 {
        return None;
    }
    let k = 1usize << w;
    let kk = k * k;
    let mut counts = vec![0u32; kk * k];
    let mut freqs = [0u32; 16];
    let mut rd = RangeDec::new(b)?;
    let mut out = vec![0u8; entries];
    let mut left = 0usize;
    for i in 0..entries {
        let above = if i >= 256 { out[i - 256] as usize } else { 0 };
        let row = (left * k + above) * k;
        let mut tot: u32 = 0;
        for (j, fj) in freqs.iter_mut().enumerate().take(k) {
            let f = 2 * counts[row + j] + 1;
            *fj = f;
            tot += f;
        }
        let s = rd.decode_symbol(&freqs[..k], tot)?;
        if s >= k {
            return None;
        }
        out[i] = s as u8;
        counts[row + s] += 1;
        left = s;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed;
        move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            s >> 33
        }
    }

    #[test]
    fn cm_roundtrip_all_widths() {
        let mut rng = lcg(0xDEADBEEF);
        for w in 1u32..=4 {
            let k = 1usize << w;
            // skewed + structured stream: dominant zeros, runs, plane noise
            let mut v: Vec<u32> = vec![0; 4096];
            for i in 0..4096 {
                let r = rng() % 100;
                v[i] = if r < 70 {
                    0
                } else if r < 85 {
                    1 % k as u64
                } else {
                    rng() % k as u64
                } as u32;
                if i >= 256 && rng() % 4 == 0 {
                    v[i] = v[i - 256]; // vertical coherence
                }
            }
            // identity remap: vanilla space == canonical space here
            let remap: Vec<u8> = (0..k as u8).collect();
            let enc = encode_canon(&v, &remap, w).expect("encode");
            let dec = decode_canon(&enc, w, 4096).expect("decode");
            assert_eq!(dec, v.iter().map(|&x| x as u8).collect::<Vec<u8>>(), "roundtrip w={w}");
        }
    }

    #[test]
    fn cm_uniform_stream_roundtrip_and_beats_dense() {
        // the W=1 uniform-ish case: CM body must stay under the dense body
        for w in 1u32..=4 {
            let v = vec![0u32; 4096];
            let remap: Vec<u8> = (0..(1usize << w) as u8).collect();
            let enc = encode_canon(&v, &remap, w).expect("encode");
            let dec = decode_canon(&enc, w, 4096).expect("decode");
            assert!(dec.iter().all(|&x| x == 0));
            let dense = (4096 * (1usize << w) + 7) / 8;
            assert!(
                enc.len() + 3 < dense,
                "uniform w={w}: cm {} + framing must beat dense {dense}",
                enc.len()
            );
        }
    }

    #[test]
    fn cm_deterministic() {
        let mut rng = lcg(42);
        let v: Vec<u32> = (0..4096).map(|_| (rng() % 4) as u32).collect();
        let remap: Vec<u8> = (0..4u8).collect();
        let a = encode_canon(&v, &remap, 2).unwrap();
        let b = encode_canon(&v, &remap, 2).unwrap();
        assert_eq!(a, b, "same content -> same bytes (dedup-stable)");
    }

    #[test]
    fn cm_rejects_out_of_range_and_bad_width() {
        let v = vec![5u32; 4096];
        let remap4: Vec<u8> = (0..4u8).collect();
        assert!(encode_canon(&v, &remap4, 2).is_none()); // 5 >= K=4
        let v = vec![1u32; 100];
        assert!(encode_canon(&v, &remap4, 2).is_none()); // not 4096
        assert!(encode_canon(&vec![0u32; 4096], &remap4, 5).is_none());
        assert!(decode_canon(&[0u8; 4], 2, 4096).is_none()); // too short
        // truncated stream must fail cleanly, not panic (the 5 flush bytes
        // are redundant, so cut deep enough to starve normalization)
        let v = vec![1u32; 4096];
        let enc = encode_canon(&v, &remap4, 2).unwrap();
        assert!(decode_canon(&enc[..enc.len() / 2], 2, 4096).is_none());
    }

    #[test]
    fn cm_nonidentity_remap_roundtrip() {
        // a non-trivial remap must be honored (encode in canonical space,
        // decode returns canonical indices)
        let mut rng = lcg(7);
        let v: Vec<u32> = (0..4096).map(|_| (rng() % 8) as u32).collect();
        let remap: Vec<u8> = vec![3, 7, 0, 5, 1, 6, 2, 4]; // a permutation of 0..8
        let enc = encode_canon(&v, &remap, 3).expect("encode");
        let dec = decode_canon(&enc, 3, 4096).expect("decode");
        for (i, &raw) in v.iter().enumerate() {
            assert_eq!(dec[i], remap[raw as usize], "canonical mismatch at {i}");
        }
    }
}
