use crate::silk::macros::{silk_mul, silk_smlawb};

pub fn silk_lin2log(in_lin: i32) -> i32 {
    if in_lin <= 0 {
        return 0;
    }

    let lz = in_lin.leading_zeros() as i32;

    let rot = 24 - lz;
    let x = in_lin as u32;
    let frac_q7 = match rot.cmp(&0) {
        std::cmp::Ordering::Equal => x & 0x7f,
        std::cmp::Ordering::Less => {
            let m = (-rot) as u32;
            x.rotate_left(m) & 0x7f
        }
        std::cmp::Ordering::Greater => {
            let r = rot as u32;
            x.rotate_right(r) & 0x7f
        }
    } as i32;

    let res = silk_smlawb(frac_q7, silk_mul(frac_q7, 128 - frac_q7), 179);
    res + ((31 - lz) << 7)
}
