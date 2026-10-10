// Codec2 700C/newamp1, adapted to fixed-storage Rust from David Rowe's
// codec2.c, newamp1.c, mbest.c, phase.c and quantise.c (Copyright 2017).
// Source commit and hashes: ../../UPSTREAM.json. LGPL-2.1-only; see NOTICE
// and licenses/LGPL-2.1.txt. No C code or allocator is linked at runtime.
use super::{codebook700c::*, *};

const K: usize = 20;
const NFFT: usize = 128;
const NS: usize = NFFT / 2 + 1;

#[derive(Clone, Debug)]
pub(super) struct State {
    frequencies: [f32; K],
    previous: [f32; K],
    wo_left: f32,
    voiced_left: i32,
    forward: kiss_fft_state<NFFT>,
    inverse: kiss_fft_state<NFFT>,
}

// Match the portable upstream POW10F configuration used by the pinned oracle.
fn pow10(value: f32) -> f32 {
    libm::expf(2.302585092994046_f32 * value)
}

// Double-expression variant preserves the C macro promotion before expf.
fn pow10_double(value: f64) -> f32 {
    libm::expf((f64::from(2.302585092994046_f32) * value) as f32)
}

impl State {
    pub(super) fn initialise(&mut self) {
        fn mel(hz: f32) -> f32 {
            libm::floorf(
                (2595.0 * f64::from(libm::log10f((1.0 + f64::from(hz) / 700.0) as f32)) + 0.5)
                    as f32,
            )
        }
        let start = mel(200.0);
        let step = (mel(3700.0) - start) / (K - 1) as f32;
        let mut value = start;
        for frequency in &mut self.frequencies {
            *frequency = (0.7 * (f64::from(pow10_double(f64::from(value) / 2595.0)) - 1.0)) as f32;
            value += step;
        }
        self.previous.fill(0.0);
        self.wo_left = 0.0;
        self.voiced_left = 0;
        self.forward.initialise(NFFT, 0);
        self.inverse.initialise(NFFT, 1);
    }
}

// Preserve upstream's parabolic interpolation and strict tie ordering. All
// callers have monotonically increasing frequency axes with at least 3 points.
fn interpolate(y: &mut [f32], xp: &[f32], yp: &[f32], x: &[f32]) {
    debug_assert!(xp.len() >= 3 && xp.len() == yp.len() && y.len() == x.len());
    let mut k = 0;
    for (yi, &xi) in y.iter_mut().zip(x) {
        while xp[k + 1] < xi && k < xp.len() - 3 {
            k += 1;
        }
        let (x1, x2, x3) = (xp[k], xp[k + 1], xp[k + 2]);
        let (y1, y2, y3) = (yp[k], yp[k + 1], yp[k + 2]);
        let a = ((y3 - y2) / (x3 - x2) - (y2 - y1) / (x2 - x1)) / (x3 - x1);
        let b = ((y3 - y2) / (x3 - x2) * (x2 - x1) + (y2 - y1) / (x2 - x1) * (x3 - x2)) / (x3 - x1);
        *yi = a * (xi - x2) * (xi - x2) + b * (xi - x2) + y2;
    }
}

fn harmonic_frequency(model: &MODEL, m: usize) -> f32 {
    (f64::from(m as f32 * model.Wo) * 4.0 / core::f64::consts::PI) as f32
}

fn rate_k(model: &MODEL, frequencies: &[f32; K]) -> [f32; K] {
    let mut amplitudes = [0.0_f32; MAX_AMP + 1];
    let mut harmonics = [0.0_f32; MAX_AMP + 1];
    let mut peak = -100.0_f32;
    for m in 1..=model.L {
        amplitudes[m] = 20.0 * libm::log10f((f64::from(model.A[m]) + 1e-16) as f32);
        peak = peak.max(amplitudes[m]);
        harmonics[m] = harmonic_frequency(model, m);
    }
    for amplitude in &mut amplitudes[1..=model.L] {
        *amplitude = amplitude.max(peak - 50.0);
    }
    let mut result = [0.0; K];
    interpolate(
        &mut result,
        &harmonics[1..=model.L],
        &amplitudes[1..=model.L],
        frequencies,
    );
    result
}

// Fixed five-candidate M-best search. Strict < preserves the first codebook
// entry on ties, including ties crossing the two-stage search order.
#[inline]
fn squared_error(vector: &[f32], target: &[f32; K], limit: f32) -> f32 {
    let mut error = 0.0;
    // Squared distances only increase. Reject a candidate once it cannot beat
    // the current cutoff, preserving both scalar addition order and ties.
    // Check in groups to avoid a branch for every DSP multiply/add.
    for start in (0..K).step_by(4) {
        for k in start..start + 4 {
            let difference = vector[k] - target[k];
            error += difference * difference;
        }
        if error >= limit {
            break;
        }
    }
    error
}

fn quantize(target: &[f32; K]) -> (usize, usize) {
    let mut best = [(1e32_f32, 0_usize); 5];
    for (index, vector) in VQ1.chunks_exact(K).enumerate() {
        let error = squared_error(vector, target, best[best.len() - 1].0);
        if let Some(position) = best.iter().position(|entry| error < entry.0) {
            for n in (position + 1..best.len()).rev() {
                best[n] = best[n - 1];
            }
            best[position] = (error, index);
        }
    }
    // Visit the flash-resident second codebook once, using each vector for all
    // five candidates while it is cached. Keep one winner per candidate, then
    // select in the original candidate order to preserve strict tie behavior.
    let mut residuals = [[0.0; K]; 5];
    let mut winners = [(1e32_f32, 0_usize); 5];
    for (rank, (_, first)) in best.iter().copied().enumerate() {
        for k in 0..K {
            residuals[rank][k] = target[k] - VQ1[first * K + k];
        }
    }
    for (second, vector) in VQ2.chunks_exact(K).enumerate() {
        for (residual, winner) in residuals.iter().zip(&mut winners) {
            let error = squared_error(vector, residual, winner.0);
            if error < winner.0 {
                *winner = (error, second);
            }
        }
    }
    let mut result = (1e32_f32, 0, 0);
    for ((_, first), (error, second)) in best.into_iter().zip(winners) {
        if error < result.0 {
            result = (error, first, second);
        }
    }
    (result.1, result.2)
}

fn encode_model(model: &MODEL, state: &State, constants: &C2const) -> [i32; 4] {
    let mut vector = rate_k(model, &state.frequencies);
    let mut sum = 0.0;
    for value in vector {
        sum += value;
    }
    let mean = sum / K as f32;
    for value in &mut vector {
        *value -= mean;
    }
    // The pinned default disables front EQ. Its unused diagnostic/EQ history
    // has no effect on the wire data and is not retained in the bounded backend.
    let (first, second) = quantize(&vector);
    let mut energy = 0;
    let mut error = f32::MAX;
    for (index, value) in ENERGY.iter().enumerate() {
        let difference = value - mean;
        let candidate = difference * difference;
        if candidate < error {
            error = candidate;
            energy = index;
        }
    }
    let pitch = if model.voiced != 0 {
        let min = constants.Wo_min.dsp_log10();
        let max = constants.Wo_max.dsp_log10();
        let normal = (model.Wo.dsp_log10() - min) / (max - min);
        (64.0 * normal + 0.5).dsp_floor().clamp(1.0, 63.0) as i32
    } else {
        0
    };
    [first as i32, second as i32, energy as i32, pitch]
}

fn decode_vector(indexes: &[i32; 4], frequencies: &[f32; K]) -> [f32; K] {
    let mut result = [0.0; K];
    let mut pre = [0.0; K];
    let (mut before, mut after) = (0.0, 0.0);
    for k in 0..K {
        result[k] = VQ1[indexes[0] as usize * K + k] + VQ2[indexes[1] as usize * K + k];
        pre[k] = 20.0 * libm::log10f((f64::from(frequencies[k]) / 0.3) as f32);
        result[k] += pre[k];
        before += pow10_double(f64::from(result[k]) / 10.0);
        result[k] *= 1.5;
        after += pow10_double(f64::from(result[k]) / 10.0);
    }
    let gain = 10.0 * (after / before).dsp_log10();
    for k in 0..K {
        result[k] -= gain;
        result[k] -= pre[k];
        result[k] += ENERGY[indexes[2] as usize];
    }
    result
}

fn rate_l(model: &mut MODEL, vector: &[f32; K], frequencies: &[f32; K]) {
    let mut levels = [0.0; K + 2];
    let mut axis = [0.0; K + 2];
    levels[1..=K].copy_from_slice(vector);
    axis[1..=K].copy_from_slice(frequencies);
    axis[K + 1] = 4.0;
    let mut harmonics = [0.0; MAX_AMP + 1];
    let mut amplitudes = [0.0; MAX_AMP + 1];
    for m in 1..=model.L {
        harmonics[m] = harmonic_frequency(model, m);
    }
    interpolate(
        &mut amplitudes[1..=model.L],
        &axis,
        &levels,
        &harmonics[1..=model.L],
    );
    for m in 1..=model.L {
        model.A[m] = pow10_double(f64::from(amplitudes[m]) / 20.0);
    }
}

// Keep the cepstral FFT scratch lifetime separate from synthesis. One 10 ms
// model is reconstructed at a time instead of retaining four models and H[].
#[inline(never)]
fn determine_phase(model: &MODEL, state: &State, h: &mut [COMP; MAX_AMP + 1]) {
    let mut amplitudes = [0.0; MAX_AMP + 1];
    let mut harmonics = [0.0; MAX_AMP + 1];
    for m in 1..=model.L {
        amplitudes[m] = 20.0 * model.A[m].dsp_log10();
        harmonics[m] = harmonic_frequency(model, m);
    }
    let mut spectrum = [0.0; NS];
    let mut frequencies = [0.0; NS];
    for (i, frequency) in frequencies.iter_mut().enumerate() {
        *frequency = 8.0 * i as f32 / NFFT as f32;
    }
    interpolate(
        &mut spectrum,
        &harmonics[1..=model.L],
        &amplitudes[1..=model.L],
        &frequencies,
    );
    let mut input = [COMP::new(); NFFT];
    let mut output = [COMP::new(); NFFT];
    input[0].r = spectrum[0];
    for i in 1..NS {
        input[i].r = spectrum[i];
        input[NFFT - i].r = spectrum[i];
    }
    kiss_fft::kiss_fft(&state.inverse, &input, &mut output);
    for value in &mut output {
        value.r /= NFFT as f32;
        value.i /= NFFT as f32;
    }
    input.fill(COMP::new());
    input[0] = output[0];
    input[NS - 1] = output[NS - 1];
    for i in 1..NS - 1 {
        input[i].r = output[i].r + output[NFFT - i].r;
        input[i].i = output[i].i + output[NFFT - i].i;
    }
    kiss_fft::kiss_fft(&state.forward, &input, &mut output);
    let scale = (20.0 / f64::from(libm::logf(10.0))) as f32;
    for m in 1..=model.L {
        let bin = libm::floorf(
            (0.5 + f64::from(m as f32 * model.Wo * NFFT as f32) / (2.0 * core::f64::consts::PI))
                as f32,
        ) as usize;
        debug_assert!(bin < NS);
        let phase = output[bin].i / scale;
        h[m].r = phase.dsp_cos();
        h[m].i = phase.dsp_sin();
    }
}

impl Codec2 {
    pub(super) fn codec2_encode_700c(&mut self, bits: &mut [u8], speech: &[i16]) {
        let mut model = MODEL::new(self.internal.c2const.p_max as f32);
        for frame in speech.chunks_exact(80) {
            self.analyse_one_frame(&mut model, frame);
        }
        let indexes = encode_model(&model, &self.internal.newamp, &self.internal.c2const);
        bits.fill(0);
        let mut offset = 0;
        for (value, width) in indexes.into_iter().zip([9, 9, 4, 6]) {
            pack_natural_or_gray(bits, &mut offset, value, width, 0);
        }
        debug_assert_eq!(offset, 28);
    }

    pub(super) fn codec2_decode_700c(&mut self, speech: &mut [i16], bits: &[u8]) {
        let mut offset = 0;
        let mut indexes = [0; 4];
        for (value, width) in indexes.iter_mut().zip([9, 9, 4, 6]) {
            *value = unpack_natural_or_gray(bits, &mut offset, width, 0);
        }
        let right = decode_vector(&indexes, &self.internal.newamp.frequencies);
        let unvoiced_wo = (2.0 * core::f64::consts::PI / 100.0) as f32;
        let voiced_right = i32::from(indexes[3] != 0);
        let wo_right = if voiced_right != 0 {
            let min = self.internal.c2const.Wo_min.dsp_log10();
            let step = (self.internal.c2const.Wo_max.dsp_log10() - min) / 64.0;
            pow10(min + step * indexes[3] as f32)
        } else {
            unvoiced_wo
        };
        for frame in 0..4 {
            let mut model = MODEL::new(self.internal.c2const.p_max as f32);
            let c = 1.0 - frame as f32 / 4.0;
            let left = &self.internal.newamp;
            match (left.voiced_left != 0, voiced_right != 0) {
                (true, true) => {
                    model.Wo = (f64::from(left.wo_left * c)
                        + f64::from(wo_right) * (1.0 - f64::from(c)))
                        as f32;
                    model.voiced = 1;
                }
                (true, false) if frame < 2 => {
                    model.Wo = left.wo_left;
                    model.voiced = 1;
                }
                (false, true) if frame >= 2 => {
                    model.Wo = wo_right;
                    model.voiced = 1;
                }
                _ => {
                    model.Wo = unvoiced_wo;
                    model.voiced = 0;
                }
            }
            model.L = libm::floorf((core::f64::consts::PI / f64::from(model.Wo)) as f32) as usize;
            let mut vector = [0.0; K];
            for k in 0..K {
                vector[k] = (f64::from(left.previous[k] * c)
                    + f64::from(right[k]) * (1.0 - f64::from(c)))
                    as f32;
            }
            rate_l(&mut model, &vector, &left.frequencies);
            let mut h = [COMP::new(); MAX_AMP + 1];
            determine_phase(&model, left, &mut h);
            self.synthesise_one_frame(
                &mut speech[80 * frame..80 * (frame + 1)],
                &mut model,
                &h,
                1.5,
            );
        }
        self.internal.newamp.previous = right;
        self.internal.newamp.wo_left = wo_right;
        self.internal.newamp.voiced_left = voiced_right;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exhaustive pre-optimization search is an independent ordering oracle.
    fn exhaustive(target: &[f32; K]) -> (usize, usize) {
        let mut best = [(1e32_f32, 0_usize); 5];
        for (index, vector) in VQ1.chunks_exact(K).enumerate() {
            let error = vector.iter().zip(target).fold(0.0, |sum, (a, b)| {
                let d = a - b;
                sum + d * d
            });
            if let Some(position) = best.iter().position(|entry| error < entry.0) {
                for n in (position + 1..best.len()).rev() {
                    best[n] = best[n - 1];
                }
                best[position] = (error, index);
            }
        }
        let mut result = (1e32_f32, 0, 0);
        for (_, first) in best {
            let mut residual = [0.0; K];
            for k in 0..K {
                residual[k] = target[k] - VQ1[first * K + k];
            }
            for (second, vector) in VQ2.chunks_exact(K).enumerate() {
                let error = vector.iter().zip(residual).fold(0.0, |sum, (a, b)| {
                    let d = a - b;
                    sum + d * d
                });
                if error < result.0 {
                    result = (error, first, second);
                }
            }
        }
        (result.1, result.2)
    }

    #[test]
    fn bounded_search_preserves_exhaustive_winners() {
        let mut random = 71_u32;
        for case in 0..4096 {
            let mut target = [0.0; K];
            for (k, value) in target.iter_mut().enumerate() {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                *value = match case % 8 {
                    0 => 0.0,
                    1 => VQ1[(case / 8 % 512) * K + k],
                    2 => VQ2[(case / 8 % 512) * K + k],
                    3 => VQ1[(case / 8 % 512) * K + k] + VQ2[((case / 8 * 17) % 512) * K + k],
                    _ => ((random >> 16) as i16) as f32 / (1 << (case % 12)) as f32,
                };
            }
            assert_eq!(quantize(&target), exhaustive(&target), "case {case}");
        }
    }

    #[test]
    fn bounded_distance_preserves_winning_arithmetic_and_ties() {
        let target = [0.0; K];
        let vector = [1.0; K];
        assert_eq!(
            squared_error(&vector, &target, 21.0).to_bits(),
            20.0_f32.to_bits()
        );
        assert_eq!(squared_error(&vector, &target, 20.0), 20.0);
        assert_eq!(squared_error(&vector, &target, 4.0), 4.0);
        assert_eq!(squared_error(&vector, &target, 5.0), 8.0);
    }
}
