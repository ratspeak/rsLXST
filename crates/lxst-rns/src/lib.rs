//! Reticulum transport boundary for LXST.
//!
//! This crate is intentionally small: it turns already-encoded LXST packets into
//! Reticulum link data packets and leaves call state, audio, and codec work to
//! higher layers.

use bytes::Bytes;
use lxst_core::{
    DropPolicy, Frame, FramePacketizer, FrameStreamEvent, FrameStreamState, JitterBuffer,
    JitterPush, JitterStats, LxstPacket, OpusDecoderState, RawAudioFrame, RawBitDepth,
};
use rns_link::link::{Link, LinkState};
use rns_transport::messages::{OutboundRequest, TransportMessage};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum Error {
    #[error("LXST core error: {0}")]
    Lxst(#[from] lxst_core::Error),
    #[error("LXST stream error: {0}")]
    Stream(#[from] lxst_core::StreamError),
    #[error("LXST Opus codec error: {0}")]
    Opus(#[from] lxst_core::OpusCodecError),
    #[error("Reticulum link is not active: {0:?}")]
    LinkNotActive(LinkState),
    #[error("LXST payload length {payload_len} exceeds link MDU {mdu}")]
    PayloadExceedsMdu { payload_len: usize, mdu: usize },
    #[error("Reticulum link encryption failed: {0}")]
    LinkEncrypt(String),
    #[error("Reticulum outbound queue is closed")]
    TransportClosed,
    #[error("Reticulum outbound queue is full")]
    TransportFull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedLinkPacket {
    pub raw: Bytes,
    pub packet_hash: [u8; 32],
    pub destination_hash: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundLxstPacket {
    pub link_id: [u8; 16],
    pub packet: LxstPacket,
    pub frame_events: Vec<FrameStreamEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaIngressResult {
    pub inbound: InboundLxstPacket,
    pub jitter_pushes: Vec<JitterPush<Frame>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LxstLinkIngress {
    frame_stream: FrameStreamState,
}

impl LxstLinkIngress {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn current_codec(&self) -> Option<lxst_core::CodecKind> {
        self.frame_stream.current_codec()
    }

    /// Decode decrypted plaintext from `LinkManager::set_link_packet_channel`.
    pub fn accept_plaintext(
        &mut self,
        link_id: [u8; 16],
        payload: &[u8],
    ) -> Result<InboundLxstPacket, Error> {
        let packet = LxstPacket::decode(payload)?;
        let frame_events = self.frame_stream.accept_packet(packet.clone());

        Ok(InboundLxstPacket {
            link_id,
            packet,
            frame_events,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LxstMediaEgress {
    packetizer: FramePacketizer,
}

impl LxstMediaEgress {
    pub const PYTHON_COMPATIBLE: Self = Self {
        packetizer: FramePacketizer::PYTHON_COMPATIBLE,
    };

    pub fn new(frames_per_packet: usize) -> Result<Self, Error> {
        Ok(Self {
            packetizer: FramePacketizer::new(frames_per_packet)?,
        })
    }

    pub const fn packetizer(&self) -> FramePacketizer {
        self.packetizer
    }

    pub fn pack_frames(
        self,
        link: &Link,
        frames: impl IntoIterator<Item = Frame>,
    ) -> Result<Vec<PackedLinkPacket>, Error> {
        self.packetizer
            .packetize(frames)
            .iter()
            .map(|packet| pack_lxst_link_packet(link, packet))
            .collect()
    }

    pub fn pack_raw_frames(
        self,
        link: &Link,
        bit_depth: RawBitDepth,
        frames: impl IntoIterator<Item = RawAudioFrame>,
    ) -> Result<Vec<PackedLinkPacket>, Error> {
        let frames = frames
            .into_iter()
            .map(|frame| frame.to_frame(bit_depth))
            .collect::<Result<Vec<_>, _>>()?;
        self.pack_frames(link, frames)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LxstMediaIngress {
    link_ingress: LxstLinkIngress,
    jitter: JitterBuffer<Frame>,
}

impl LxstMediaIngress {
    pub fn new(jitter_capacity: usize, drop_policy: DropPolicy) -> Result<Self, Error> {
        Ok(Self {
            link_ingress: LxstLinkIngress::new(),
            jitter: JitterBuffer::new(jitter_capacity, drop_policy)?,
        })
    }

    pub const fn current_codec(&self) -> Option<lxst_core::CodecKind> {
        self.link_ingress.current_codec()
    }

    pub const fn jitter_stats(&self) -> JitterStats {
        self.jitter.stats()
    }

    pub fn jitter_len(&self) -> usize {
        self.jitter.len()
    }

    pub fn accept_plaintext(
        &mut self,
        link_id: [u8; 16],
        payload: &[u8],
    ) -> Result<MediaIngressResult, Error> {
        let inbound = self.link_ingress.accept_plaintext(link_id, payload)?;
        let mut jitter_pushes = Vec::new();

        for event in &inbound.frame_events {
            if let FrameStreamEvent::Frame(frame) = event {
                jitter_pushes.push(self.jitter.push(frame.clone()));
            }
        }

        Ok(MediaIngressResult {
            inbound,
            jitter_pushes,
        })
    }

    pub fn pop_frame(&mut self) -> Option<Frame> {
        self.jitter.pop()
    }

    pub fn pop_raw_frame(&mut self) -> Result<Option<RawAudioFrame>, Error> {
        self.pop_frame()
            .map(|frame| RawAudioFrame::from_frame(&frame).map_err(Error::from))
            .transpose()
    }

    pub fn pop_opus_frame(
        &mut self,
        decoder: &mut OpusDecoderState,
    ) -> Result<Option<RawAudioFrame>, Error> {
        self.pop_frame()
            .map(|frame| decoder.decode_frame(&frame).map_err(Error::from))
            .transpose()
    }
}

/// Encode and encrypt an LXST packet for transmission over an active Reticulum
/// link as a no-receipt data packet.
pub fn pack_lxst_link_packet(link: &Link, packet: &LxstPacket) -> Result<PackedLinkPacket, Error> {
    let payload = packet.encode()?;
    pack_link_payload(link, &payload)
}

/// Encrypt application payload bytes and wrap them in a Reticulum LINK/DATA
/// packet with context NONE, matching Python `RNS.Packet(link, data,
/// create_receipt=False)`.
pub fn pack_link_payload(link: &Link, payload: &[u8]) -> Result<PackedLinkPacket, Error> {
    if link.state != LinkState::Active {
        return Err(Error::LinkNotActive(link.state));
    }

    if payload.len() > link.mdu {
        return Err(Error::PayloadExceedsMdu {
            payload_len: payload.len(),
            mdu: link.mdu,
        });
    }

    let encrypted = link
        .encrypt(payload)
        .map_err(|e| Error::LinkEncrypt(e.to_string()))?;
    let header = rns_wire::header::PacketHeader {
        flags: rns_wire::flags::PacketFlags {
            header_type: rns_wire::flags::HeaderType::Header1,
            context_flag: false,
            transport_type: rns_wire::flags::TransportType::Broadcast,
            destination_type: rns_wire::flags::DestinationType::Link,
            packet_type: rns_wire::flags::PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: link.link_id,
        context: rns_wire::context::PacketContext::None,
    };

    let mut raw = header.pack();
    raw.extend_from_slice(&encrypted);
    let packet_hash = rns_wire::hash::packet_hash(&raw, rns_wire::flags::HeaderType::Header1);

    Ok(PackedLinkPacket {
        raw: Bytes::from(raw),
        packet_hash,
        destination_hash: link.link_id,
    })
}

/// Pack and queue an already-encoded LXST payload for Reticulum transmission.
pub fn queue_link_payload(
    transport_tx: &mpsc::Sender<TransportMessage>,
    link: &Link,
    payload: &[u8],
) -> Result<PackedLinkPacket, Error> {
    let packet = pack_link_payload(link, payload)?;
    queue_packed_link_packet(transport_tx, packet)
}

/// Pack and queue a structured LXST packet for Reticulum transmission.
pub fn queue_lxst_link_packet(
    transport_tx: &mpsc::Sender<TransportMessage>,
    link: &Link,
    packet: &LxstPacket,
) -> Result<PackedLinkPacket, Error> {
    let packet = pack_lxst_link_packet(link, packet)?;
    queue_packed_link_packet(transport_tx, packet)
}

fn queue_packed_link_packet(
    transport_tx: &mpsc::Sender<TransportMessage>,
    packet: PackedLinkPacket,
) -> Result<PackedLinkPacket, Error> {
    transport_tx
        .try_send(TransportMessage::Outbound(OutboundRequest {
            raw: packet.raw.clone(),
            destination_hash: packet.destination_hash,
        }))
        .map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => Error::TransportFull,
            mpsc::error::TrySendError::Closed(_) => Error::TransportClosed,
        })?;

    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lxst_core::{
        CodecKind, DropPolicy, Frame, FrameStreamEvent, OpusDecoderState, OpusEncoderState,
        Profile, Signal, SignallingStatus, SyntheticSource, SyntheticSourceKind,
    };
    use rns_crypto::ed25519::Ed25519PrivateKey;

    fn active_link_pair() -> (Link, Link) {
        let dest_hash = [0xAA; 16];
        let identity_key = Ed25519PrivateKey::generate();
        let identity_pub = identity_key.public_key();

        let (mut initiator, request_data) = Link::new_initiator(dest_hash, 1);
        let (mut responder, proof_data) =
            Link::new_responder(&request_data, &identity_key, dest_hash, 1).unwrap();

        let rtt_data = initiator
            .validate_proof(&proof_data, &identity_pub, &identity_pub.to_bytes())
            .unwrap();
        responder.receive_rtt_packet(&rtt_data).unwrap();

        (initiator, responder)
    }

    #[test]
    fn packed_lxst_packet_decrypts_on_peer_link() {
        let (initiator, responder) = active_link_pair();
        let lxst = LxstPacket::signalling([
            Signal::from(SignallingStatus::Available),
            Signal::from(Profile::QualityMedium),
        ]);

        let packet = pack_lxst_link_packet(&initiator, &lxst).unwrap();
        let (header, data_offset) = rns_wire::header::PacketHeader::unpack(&packet.raw).unwrap();

        assert_eq!(
            header.flags.destination_type,
            rns_wire::flags::DestinationType::Link
        );
        assert_eq!(header.flags.packet_type, rns_wire::flags::PacketType::Data);
        assert_eq!(header.context, rns_wire::context::PacketContext::None);
        assert_eq!(header.destination_hash, initiator.link_id);

        let decrypted = responder.decrypt(&packet.raw[data_offset..]).unwrap();
        assert_eq!(LxstPacket::decode(&decrypted).unwrap(), lxst);
    }

    #[test]
    fn queue_sends_outbound_transport_message() {
        let (initiator, _responder) = active_link_pair();
        let payload = LxstPacket::frame(Frame::new(CodecKind::Raw, [0x00, 0x11, 0x22]))
            .encode()
            .unwrap();
        let (tx, mut rx) = mpsc::channel(1);

        let packet = queue_link_payload(&tx, &initiator, &payload).unwrap();
        let sent = rx.try_recv().unwrap();
        let TransportMessage::Outbound(outbound) = sent else {
            panic!("expected outbound transport message");
        };

        assert_eq!(outbound.raw, packet.raw);
        assert_eq!(outbound.destination_hash, initiator.link_id);
    }

    #[test]
    fn inactive_links_do_not_pack_media_packets() {
        let (link, _request_data) = Link::new_initiator([0xBB; 16], 1);
        let packet = LxstPacket::signalling([Signal::from(SignallingStatus::Calling)]);

        assert!(matches!(
            pack_lxst_link_packet(&link, &packet),
            Err(Error::LinkNotActive(LinkState::Pending))
        ));
    }

    #[test]
    fn payloads_must_fit_link_mdu() {
        let (initiator, _responder) = active_link_pair();
        let payload = vec![0u8; initiator.mdu + 1];

        assert!(matches!(
            pack_link_payload(&initiator, &payload),
            Err(Error::PayloadExceedsMdu { .. })
        ));
    }

    #[test]
    fn ingress_decodes_decrypted_link_payloads_and_tracks_codecs() {
        let link_id = [0x44; 16];
        let mut ingress = LxstLinkIngress::new();
        let packet = LxstPacket {
            signals: vec![Signal::from(SignallingStatus::Established)],
            frames: vec![
                Frame::new(CodecKind::Raw, [0x00, 0x11]),
                Frame::new(CodecKind::Opus, [0xF8, 0xFF, 0xFE]),
            ],
        };

        let inbound = ingress
            .accept_plaintext(link_id, &packet.encode().unwrap())
            .unwrap();

        assert_eq!(inbound.link_id, link_id);
        assert_eq!(inbound.packet.signals, packet.signals);
        assert_eq!(
            inbound.frame_events,
            vec![
                FrameStreamEvent::CodecChanged {
                    from: None,
                    to: CodecKind::Raw,
                },
                FrameStreamEvent::Frame(Frame::new(CodecKind::Raw, [0x00, 0x11])),
                FrameStreamEvent::CodecChanged {
                    from: Some(CodecKind::Raw),
                    to: CodecKind::Opus,
                },
                FrameStreamEvent::Frame(Frame::new(CodecKind::Opus, [0xF8, 0xFF, 0xFE])),
            ]
        );
        assert_eq!(ingress.current_codec(), Some(CodecKind::Opus));
    }

    #[test]
    fn ingress_rejects_malformed_plaintext() {
        let mut ingress = LxstLinkIngress::new();
        assert!(matches!(
            ingress.accept_plaintext([0x55; 16], b"not msgpack"),
            Err(Error::Lxst(_))
        ));
    }

    #[test]
    fn media_egress_and_ingress_roundtrip_raw_frames_over_link_crypto() {
        let (initiator, responder) = active_link_pair();
        let raw_frames = vec![
            RawAudioFrame::new(2, vec![0.0, 0.5, -0.25, 1.0]).unwrap(),
            RawAudioFrame::new(2, vec![0.125, -0.125, 0.75, -0.75]).unwrap(),
        ];

        let packed = LxstMediaEgress::PYTHON_COMPATIBLE
            .pack_raw_frames(&initiator, RawBitDepth::Float16, raw_frames.clone())
            .unwrap();
        assert_eq!(packed.len(), raw_frames.len());

        let mut ingress = LxstMediaIngress::new(4, DropPolicy::DropOldest).unwrap();
        for packet in packed {
            let (_header, data_offset) = rns_wire::header::PacketHeader::unpack(&packet.raw)
                .expect("packed Reticulum header");
            let plaintext = responder.decrypt(&packet.raw[data_offset..]).unwrap();
            let result = ingress
                .accept_plaintext(responder.link_id, &plaintext)
                .expect("accept LXST payload");
            assert_eq!(result.jitter_pushes, vec![JitterPush::Accepted]);
        }

        assert_eq!(ingress.current_codec(), Some(CodecKind::Raw));
        assert_eq!(ingress.jitter_len(), raw_frames.len());
        assert_eq!(
            ingress.pop_raw_frame().unwrap(),
            Some(raw_frames[0].clone())
        );
        assert_eq!(
            ingress.pop_raw_frame().unwrap(),
            Some(raw_frames[1].clone())
        );
        assert_eq!(ingress.pop_raw_frame().unwrap(), None);
        assert_eq!(
            ingress.jitter_stats(),
            JitterStats {
                pushed: 2,
                popped: 2,
                dropped_oldest: 0,
                dropped_newest: 0,
                underruns: 1,
            }
        );
    }

    #[test]
    fn media_egress_can_batch_frames_for_rust_peers() {
        let (initiator, responder) = active_link_pair();
        let raw_frames = vec![
            RawAudioFrame::new(1, vec![0.0]).unwrap(),
            RawAudioFrame::new(1, vec![0.5]).unwrap(),
            RawAudioFrame::new(1, vec![1.0]).unwrap(),
        ];

        let packed = LxstMediaEgress::new(2)
            .unwrap()
            .pack_raw_frames(&initiator, RawBitDepth::Float32, raw_frames.clone())
            .unwrap();
        assert_eq!(packed.len(), 2);

        let mut ingress = LxstMediaIngress::new(8, DropPolicy::DropNewest).unwrap();
        for packet in packed {
            let (_header, data_offset) = rns_wire::header::PacketHeader::unpack(&packet.raw)
                .expect("packed Reticulum header");
            let plaintext = responder.decrypt(&packet.raw[data_offset..]).unwrap();
            ingress
                .accept_plaintext(responder.link_id, &plaintext)
                .expect("accept LXST payload");
        }

        assert_eq!(
            ingress.pop_raw_frame().unwrap(),
            Some(raw_frames[0].clone())
        );
        assert_eq!(
            ingress.pop_raw_frame().unwrap(),
            Some(raw_frames[1].clone())
        );
        assert_eq!(
            ingress.pop_raw_frame().unwrap(),
            Some(raw_frames[2].clone())
        );
    }

    #[test]
    fn media_ingress_jitter_policy_drops_oldest_under_pressure() {
        let (initiator, responder) = active_link_pair();
        let raw_frames = vec![
            RawAudioFrame::new(1, vec![0.0]).unwrap(),
            RawAudioFrame::new(1, vec![1.0]).unwrap(),
        ];
        let packed = LxstMediaEgress::PYTHON_COMPATIBLE
            .pack_raw_frames(&initiator, RawBitDepth::Float16, raw_frames.clone())
            .unwrap();
        let mut ingress = LxstMediaIngress::new(1, DropPolicy::DropOldest).unwrap();

        let mut push_results = Vec::new();
        for packet in packed {
            let (_header, data_offset) = rns_wire::header::PacketHeader::unpack(&packet.raw)
                .expect("packed Reticulum header");
            let plaintext = responder.decrypt(&packet.raw[data_offset..]).unwrap();
            let result = ingress
                .accept_plaintext(responder.link_id, &plaintext)
                .unwrap();
            push_results.extend(result.jitter_pushes);
        }

        assert!(matches!(push_results[0], JitterPush::Accepted));
        assert!(matches!(push_results[1], JitterPush::DroppedOldest(_)));
        assert_eq!(
            ingress.pop_raw_frame().unwrap(),
            Some(raw_frames[1].clone())
        );
        assert_eq!(
            ingress.jitter_stats(),
            JitterStats {
                pushed: 2,
                popped: 1,
                dropped_oldest: 1,
                dropped_newest: 0,
                underruns: 0,
            }
        );
    }

    #[test]
    fn synthetic_raw_source_survives_link_media_flow() {
        let (initiator, responder) = active_link_pair();
        let mut source = SyntheticSource::new(
            2,
            48_000,
            4,
            SyntheticSourceKind::Ramp {
                start: -0.5,
                step: 0.125,
            },
        )
        .unwrap();
        let expected = (0..12)
            .map(|_| source.next_raw_frame().unwrap())
            .collect::<Vec<_>>();

        let packed = LxstMediaEgress::PYTHON_COMPATIBLE
            .pack_raw_frames(&initiator, RawBitDepth::Float32, expected.clone())
            .unwrap();
        let mut ingress = LxstMediaIngress::new(16, DropPolicy::DropOldest).unwrap();

        for packet in packed {
            let (_header, data_offset) =
                rns_wire::header::PacketHeader::unpack(&packet.raw).unwrap();
            let plaintext = responder.decrypt(&packet.raw[data_offset..]).unwrap();
            ingress
                .accept_plaintext(responder.link_id, &plaintext)
                .unwrap();
        }

        let mut actual = Vec::new();
        while let Some(frame) = ingress.pop_raw_frame().unwrap() {
            actual.push(frame);
        }

        assert_eq!(actual, expected);
        assert_eq!(
            ingress.jitter_stats(),
            JitterStats {
                pushed: 12,
                popped: 12,
                dropped_oldest: 0,
                dropped_newest: 0,
                underruns: 1,
            }
        );
    }

    #[test]
    fn sustained_opus_source_survives_link_media_flow() {
        let (initiator, responder) = active_link_pair();
        let profile = Profile::QualityHigh;
        let mut source = SyntheticSource::new(
            profile.channels(),
            profile.sample_rate_hz(),
            profile.sample_frames_per_packet(),
            SyntheticSourceKind::Sine {
                frequency_hz: 440.0,
                amplitude: 0.25,
            },
        )
        .unwrap();
        let raw_frames = (0..12)
            .map(|_| source.next_raw_frame().unwrap())
            .collect::<Vec<_>>();
        let mut encoder = OpusEncoderState::new(profile).unwrap();
        let opus_frames = raw_frames
            .iter()
            .map(|frame| encoder.encode_frame(frame).unwrap())
            .collect::<Vec<_>>();

        let packed = LxstMediaEgress::PYTHON_COMPATIBLE
            .pack_frames(&initiator, opus_frames)
            .unwrap();
        assert_eq!(packed.len(), raw_frames.len());

        let mut ingress = LxstMediaIngress::new(16, DropPolicy::DropOldest).unwrap();
        for packet in packed {
            let (_header, data_offset) =
                rns_wire::header::PacketHeader::unpack(&packet.raw).unwrap();
            let plaintext = responder.decrypt(&packet.raw[data_offset..]).unwrap();
            let result = ingress
                .accept_plaintext(responder.link_id, &plaintext)
                .unwrap();
            assert_eq!(result.jitter_pushes, vec![JitterPush::Accepted]);
        }

        let mut decoder = OpusDecoderState::new(profile).unwrap();
        let mut decoded_frames = Vec::new();
        while let Some(frame) = ingress.pop_opus_frame(&mut decoder).unwrap() {
            decoded_frames.push(frame);
        }

        assert_eq!(decoded_frames.len(), raw_frames.len());
        assert_eq!(ingress.current_codec(), Some(CodecKind::Opus));
        for frame in decoded_frames {
            assert_eq!(frame.channels, profile.channels());
            assert_eq!(frame.sample_frames(), profile.sample_frames_per_packet());
        }
        assert_eq!(
            ingress.jitter_stats(),
            JitterStats {
                pushed: 12,
                popped: 12,
                dropped_oldest: 0,
                dropped_newest: 0,
                underruns: 1,
            }
        );
    }
}
