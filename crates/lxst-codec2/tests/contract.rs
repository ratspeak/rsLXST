use lxst_codec2::{Codec, Error, Mode};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Meter;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;

// One process/thread: unrelated parallel test allocations cannot contaminate
// the count. Large states are allocated before measurement and initialised in place.
fn main() {
    for mode in [Mode::Rate1600, Mode::Rate3200] {
        let mut duplex_storage = Box::<Codec>::new_uninit();
        let mut encoder_storage = Box::<Codec>::new_uninit();
        let mut decoder_storage = Box::<Codec>::new_uninit();
        let before = ALLOCATIONS.load(Relaxed);
        let duplex = Codec::initialise(&mut duplex_storage, mode);
        let encoder = Codec::initialise(&mut encoder_storage, mode);
        let decoder = Codec::initialise(&mut decoder_storage, mode);
        let samples = mode.samples();
        let mut input = [0_i16; 320];
        let mut expected = [0_i16; 320];
        let mut actual = [0_i16; 320];
        let mut encoded = [0_u8; 8];
        let mut same = [0_u8; 8];
        let mut rng = 71_u32;
        for frame in 0..1000 {
            for sample in &mut input[..samples] {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                *sample = match frame % 5 {
                    0 => 0,
                    1 => (rng >> 16) as i16,
                    2 => {
                        if rng & 1 == 0 {
                            i16::MIN
                        } else {
                            i16::MAX
                        }
                    }
                    _ => ((rng >> 16) as i16) / 4,
                };
            }
            // Rejected input cannot alter state or caller outputs.
            same.fill(0xA5);
            assert_eq!(
                duplex.encode(&input[..samples - 1], &mut same),
                Err(Error::PcmLength)
            );
            assert_eq!(same, [0xA5; 8]);
            assert_eq!(
                duplex.encode(&input[..samples], &mut same[..7]),
                Err(Error::EncodedLength)
            );
            actual.fill(123);
            assert_eq!(
                duplex.decode(&encoded[..7], &mut actual[..samples]),
                Err(Error::EncodedLength)
            );
            assert_eq!(
                duplex.decode(&encoded, &mut actual[..samples - 1]),
                Err(Error::PcmLength)
            );
            assert!(actual.iter().all(|v| *v == 123));
            encoder.encode(&input[..samples], &mut encoded).unwrap();
            duplex.encode(&input[..samples], &mut same).unwrap();
            assert_eq!(encoded, same, "receive activity changed encoder history");
            decoder.decode(&encoded, &mut expected[..samples]).unwrap();
            duplex.decode(&encoded, &mut actual[..samples]).unwrap();
            assert_eq!(
                expected[..samples],
                actual[..samples],
                "another decoder changed random/history state"
            );
            if frame % 127 == 0 {
                duplex.reset(mode);
                encoder.reset(mode);
                decoder.reset(mode);
            }
        }
        // Every 64-bit payload is valid codec data. Exercise decoder indices on
        // hostile but correctly-sized frames, including both extrema.
        for frame in 0..10000 {
            for byte in &mut encoded {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                *byte = match frame % 10 {
                    0 => 0,
                    1 => 255,
                    _ => (rng >> 24) as u8,
                };
            }
            duplex.decode(&encoded, &mut actual[..samples]).unwrap();
        }
        assert_eq!(
            ALLOCATIONS.load(Relaxed),
            before,
            "initialisation/reset/stream allocated"
        );
        let other = match mode {
            Mode::Rate1600 => Mode::Rate3200,
            Mode::Rate3200 => Mode::Rate1600,
        };
        duplex.reset(other);
        assert_eq!(duplex.mode(), other);
        assert_eq!(ALLOCATIONS.load(Relaxed), before);
        println!(
            "{mode:?}: allocation-free construction/reset; isolated duplex histories; 1000 stream pairs; 10000 arbitrary decoder frames PASS"
        );
    }
    println!(
        "codec fixed storage: {} bytes, alignment {}",
        size_of::<Codec>(),
        align_of::<Codec>()
    );
}
