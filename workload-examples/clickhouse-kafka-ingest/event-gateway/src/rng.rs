//! Randomness for synthetic traffic.
//!
//! Seeded once per request from `wasi:random`, then advanced with xorshift64*
//! rather than calling the host for every value. Generating a 5,000-event
//! batch is 5,000 * ~10 draws; that is a lot of host calls for data whose only
//! consumer is a demo dashboard. Identifiers are drawn the same way, so
//! nothing here should be used where unpredictability matters.

pub(crate) struct Rng(u64);

impl Rng {
    /// Seeds from the host CSPRNG.
    pub(crate) fn from_entropy() -> Self {
        // `random0_2_0`, not `random`: the wasi:http@0.3.0 package pulls
        // wasi:random@0.3.0 in alongside the 0.2.0 this world imports, so
        // wit-bindgen suffixes both module paths to disambiguate.
        let bytes = crate::bindings::wasi::random0_2_0::random::get_random_bytes(8);
        let mut seed = [0u8; 8];
        for (slot, byte) in seed.iter_mut().zip(bytes.iter()) {
            *slot = *byte;
        }
        // A zero seed is a fixed point for xorshift, so nudge it.
        Self(u64::from_le_bytes(seed) | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-enough in `[0, n)`. Modulo bias is irrelevant at these bounds.
    pub(crate) fn below(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        (self.next_u64() >> 32) as u32 % n
    }

    /// A v4-shaped UUID. Shaped, not cryptographically random: see the module
    /// note. Real event ids should come from the client that observed the
    /// event anyway, so this is only a fallback.
    pub(crate) fn uuid_v4(&mut self) -> String {
        let (hi, lo) = (self.next_u64(), self.next_u64());
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&hi.to_le_bytes());
        b[8..].copy_from_slice(&lo.to_le_bytes());
        b[6] = (b[6] & 0x0f) | 0x40; // version 4
        b[8] = (b[8] & 0x3f) | 0x80; // variant 1
        let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
        format!(
            "{}-{}-{}-{}-{}",
            h(&b[0..4]),
            h(&b[4..6]),
            h(&b[6..8]),
            h(&b[8..10]),
            h(&b[10..16])
        )
    }

    /// A short prefixed identifier, e.g. `sess-3f9a1c22`.
    pub(crate) fn id(&mut self, prefix: &str) -> String {
        format!("{prefix}-{:08x}", self.next_u64() as u32)
    }
}
