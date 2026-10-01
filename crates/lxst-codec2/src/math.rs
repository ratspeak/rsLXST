// Same pure-Rust libm functions on host and MCU. Do not replace with approximate
// intrinsics without comparing stateful speech and target execution budgets.
pub(crate) trait FloatMath {
    fn dsp_acos(self) -> Self;
    fn dsp_atan2(self, other: Self) -> Self;
    fn dsp_ceil(self) -> Self;
    fn dsp_cos(self) -> Self;
    fn dsp_floor(self) -> Self;
    fn dsp_log10(self) -> Self;
    fn dsp_powf(self, other: Self) -> Self;
    fn dsp_round(self) -> Self;
    fn dsp_sin(self) -> Self;
    fn dsp_sqrt(self) -> Self;
}

impl FloatMath for f32 {
    fn dsp_acos(self) -> Self {
        libm::acosf(self)
    }
    fn dsp_atan2(self, other: Self) -> Self {
        libm::atan2f(self, other)
    }
    fn dsp_ceil(self) -> Self {
        libm::ceilf(self)
    }
    fn dsp_cos(self) -> Self {
        libm::cosf(self)
    }
    fn dsp_floor(self) -> Self {
        libm::floorf(self)
    }
    fn dsp_log10(self) -> Self {
        libm::log10f(self)
    }
    fn dsp_powf(self, other: Self) -> Self {
        libm::powf(self, other)
    }
    fn dsp_round(self) -> Self {
        libm::roundf(self)
    }
    fn dsp_sin(self) -> Self {
        libm::sinf(self)
    }
    fn dsp_sqrt(self) -> Self {
        libm::sqrtf(self)
    }
}
