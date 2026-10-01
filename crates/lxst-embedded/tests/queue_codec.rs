use lxst_embedded::{Codec2PacketCodec, MediaQueue, Profile, QueueError};
#[test]
fn stalled_media_drops_old_frames_and_never_crosses_generations() {
    let mut queue = MediaQueue::new();
    for value in 1..=4 {
        queue.push(&[value, value], 7, 10, 100).unwrap();
    }
    assert_eq!(queue.len(), 3);
    assert_eq!(queue.dropped(), 1);
    let mut output = [0xa5; 8];
    assert_eq!(
        queue.pop(11, 7, &mut output[..1]),
        Err(QueueError::OutputCapacity)
    );
    assert_eq!(queue.len(), 3);
    assert_eq!(output, [0xa5; 8]);
    assert_eq!(queue.pop(12, 7, &mut output), Ok(Some(2)));
    assert_eq!(output[..2], [2, 2]);
    assert_eq!(queue.pop(100, 7, &mut output), Ok(None));
    assert_eq!(queue.dropped(), 3);
    queue.push(&[5], 7, 100, 110).unwrap();
    assert_eq!(queue.pop(101, 8, &mut output), Ok(None));
    assert_eq!(queue.push(&[6], 8, 110, 110), Err(QueueError::Expired));
    queue.push(&[7], 8, 120, 130).unwrap();
    queue.clear();
    assert_eq!(queue.pop(121, 8, &mut output), Ok(None));
    assert_eq!(queue.dropped(), 4);
}
#[test]
fn borrowed_native_backend_runs_shared_packet_adapter_without_state_moves() {
    for (mode, profile, bytes, samples) in [
        (
            lxst_codec2::Mode::Rate1600,
            Profile::BandwidthVeryLow,
            65,
            2560,
        ),
        (lxst_codec2::Mode::Rate3200, Profile::BandwidthLow, 81, 1600),
    ] {
        let mut storage = Box::<lxst_codec2::Codec>::new_uninit();
        let native = lxst_codec2::Codec::initialise(&mut storage, mode);
        let mut codec = Codec2PacketCodec::new(profile, native).unwrap();
        let mut output = [0xa5; 90];
        let pcm = [0; 2560];
        assert_eq!(
            codec.encode_into(&pcm[..samples], &mut output).unwrap(),
            bytes
        );
        let mut decoded = [123; 2568];
        assert_eq!(
            codec.decode_into(&output[..bytes], &mut decoded).unwrap(),
            samples
        );
        assert!(decoded[samples..].iter().all(|v| *v == 123));
        assert!(output[bytes..].iter().all(|v| *v == 0xa5));
    }
}
