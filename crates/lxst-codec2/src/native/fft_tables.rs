// FFT coefficients depend only on transform size/direction. Keep them in
// read-only flash; each stream retains just zero-valid integer selectors.
// Histories and transform scratch remain private to their Codec instance.
use super::{fft_coefficients::*, inner::kiss_fft_cpx};
use core::ops::Deref;

#[derive(Clone, Debug)]
pub(super) struct ComplexTable {
    length: usize,
    inverse: bool,
}
impl ComplexTable {
    pub(super) fn initialise(&mut self, length: usize, inverse: bool) {
        assert!(matches!(length, 128 | 256 | 512));
        self.length = length;
        self.inverse = inverse;
    }
}
impl Deref for ComplexTable {
    type Target = [kiss_fft_cpx];
    fn deref(&self) -> &Self::Target {
        match (self.length, self.inverse) {
            (0, _) => &[],
            (128, false) => &COMPLEX_128_FORWARD,
            (128, true) => &COMPLEX_128_INVERSE,
            (256, false) => &COMPLEX_256_FORWARD,
            (256, true) => &COMPLEX_256_INVERSE,
            (512, false) => &COMPLEX_512_FORWARD,
            (512, true) => &COMPLEX_512_INVERSE,
            _ => unreachable!("invalid fixed FFT table"),
        }
    }
}
#[derive(Clone, Debug)]
pub(super) struct RealTable {
    nfft: usize,
    inverse: bool,
}
impl RealTable {
    pub(super) fn initialise(&mut self, nfft: usize, inverse: bool) {
        assert!(matches!(nfft, 128 | 256));
        self.nfft = nfft;
        self.inverse = inverse;
    }
}
impl Deref for RealTable {
    type Target = [kiss_fft_cpx];
    fn deref(&self) -> &Self::Target {
        match (self.nfft, self.inverse) {
            (0, _) => &[],
            (128, false) => &REAL_128_FORWARD,
            (128, true) => &REAL_128_INVERSE,
            (256, false) => &REAL_256_FORWARD,
            (256, true) => &REAL_256_INVERSE,
            _ => unreachable!("invalid fixed real FFT table"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Compare every stored bit to the original runtime initialiser, including
    // signed zero and forward/inverse phase operation order. No approximations.
    #[test]
    fn flash_tables_match_native_initialisation() {
        for inverse in [false, true] {
            for nfft in [128, 256, 512] {
                let mut table = ComplexTable {
                    length: 0,
                    inverse: false,
                };
                assert!(table.is_empty());
                table.initialise(nfft, inverse);
                assert_eq!(table.len(), nfft);
                for (i, value) in table.iter().enumerate() {
                    let mut phase = -2.0 * core::f64::consts::PI * i as f64 / nfft as f64;
                    if inverse {
                        phase *= -1.0;
                    }
                    let original = kiss_fft_cpx::kf_cexp(phase as f32);
                    assert_eq!(
                        (value.r.to_bits(), value.i.to_bits()),
                        (original.r.to_bits(), original.i.to_bits())
                    );
                }
            }
            for nfft in [128, 256] {
                let mut table = RealTable {
                    nfft: 0,
                    inverse: false,
                };
                assert!(table.is_empty());
                table.initialise(nfft, inverse);
                assert_eq!(table.len(), nfft / 2);
                for (i, value) in table.iter().enumerate() {
                    let mut phase = (-core::f64::consts::PI
                        * (f64::from((i + 1) as f32 / nfft as f32) + 0.5))
                        as f32;
                    if inverse {
                        phase *= -1.0;
                    }
                    let original = kiss_fft_cpx::kf_cexp(phase);
                    assert_eq!(
                        (value.r.to_bits(), value.i.to_bits()),
                        (original.r.to_bits(), original.i.to_bits())
                    );
                }
            }
        }
        assert!(core::mem::size_of::<crate::Codec>() < 17000);
    }
}
