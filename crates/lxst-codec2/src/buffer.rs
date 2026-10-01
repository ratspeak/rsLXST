use core::ops::{Deref, DerefMut};

/// Private fixed storage preserving the upstream DSP's slice lengths.
/// Zero initialisation is valid: a zero length exposes no uninitialised data.
#[derive(Clone, Debug)]
pub(crate) struct Buffer<T: Copy, const N: usize> {
    length: usize,
    values: [T; N],
}

impl<T: Copy, const N: usize> Buffer<T, N> {
    pub(crate) fn filled(length: usize, value: T) -> Self {
        assert!(length <= N);
        Self {
            length,
            values: [value; N],
        }
    }

    pub(crate) fn initialise(&mut self, length: usize, value: T) {
        assert!(length <= N);
        self.length = length;
        self.values[..length].fill(value);
    }
}

impl<T: Copy, const N: usize> Deref for Buffer<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.values[..self.length]
    }
}

impl<T: Copy, const N: usize> DerefMut for Buffer<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.values[..self.length]
    }
}
