//! MPEG-TS muxing.
//!
//! Ours rather than a resident ffmpeg fed over pipes, for two reasons: timestamps come from the
//! QPC clock both capture legs already share instead of from "both streams started at about the
//! same time", and there is no second process sitting in memory for the life of the session.
//!
//! TS is the container because a replay buffer needs segments that can be **concatenated as raw
//! bytes**. Every segment starts with its own PAT/PMT, so joining two is `cat` — the same property
//! HLS relies on. Fragmented MP4 would need its init segment reconciled on every join.
//!
//! Only the subset we actually emit is implemented: one program, one video stream, optionally one
//! audio stream, no scrambling, no descriptors.

/// Fixed PIDs. Nothing else shares this transport, so there is no reason to allocate them.
const PID_PAT: u16 = 0x0000;
const PID_PMT: u16 = 0x1000;
pub const PID_VIDEO: u16 = 0x0100;
pub const PID_AUDIO: u16 = 0x0101;

const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_AAC_ADTS: u8 = 0x0F;

const PACKET: usize = 188;
const PAYLOAD: usize = PACKET - 4;

pub struct TsMuxer {
    has_audio: bool,
    cc_pat: u8,
    cc_pmt: u8,
    cc_video: u8,
    cc_audio: u8,
    /// Sequence header from the first keyframe, kept so it can be re-inserted for encoders that do
    /// not repeat it. NVENC does; not every vendor does.
    sequence_header: Option<Vec<u8>>,
}

impl TsMuxer {
    pub fn new(has_audio: bool) -> Self {
        TsMuxer {
            has_audio,
            cc_pat: 0,
            cc_pmt: 0,
            cc_video: 0,
            cc_audio: 0,
            sequence_header: None,
        }
    }

    /// PAT and PMT. Written at the head of every segment so each one stands alone.
    pub fn write_tables(&mut self, out: &mut Vec<u8>) {
        let mut pat = Vec::new();
        pat.push(0x00); // pointer field
        let mut section = vec![
            0x00, // table_id: program association
        ];
        let body = [
            0x00, 0x01, // transport_stream_id
            0xC1, // reserved, version 0, current
            0x00, 0x00, // section_number, last_section_number
            0x00, 0x01, // program_number 1
            0xE0 | ((PID_PMT >> 8) as u8),
            (PID_PMT & 0xFF) as u8,
        ];
        let length = body.len() + 4; // + CRC
        section.push(0xB0 | ((length >> 8) as u8));
        section.push((length & 0xFF) as u8);
        section.extend_from_slice(&body);
        let crc = crc32(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        pat.extend_from_slice(&section);
        emit(out, PID_PAT, &mut self.cc_pat, &pat, None, false);

        let mut pmt = Vec::new();
        pmt.push(0x00);
        let mut streams = vec![
            STREAM_TYPE_H264,
            0xE0 | ((PID_VIDEO >> 8) as u8),
            (PID_VIDEO & 0xFF) as u8,
            0xF0,
            0x00, // ES_info_length
        ];
        if self.has_audio {
            streams.extend_from_slice(&[
                STREAM_TYPE_AAC_ADTS,
                0xE0 | ((PID_AUDIO >> 8) as u8),
                (PID_AUDIO & 0xFF) as u8,
                0xF0,
                0x00,
            ]);
        }

        let mut body = vec![
            0x00, 0x01, // program_number
            0xC1, // reserved, version 0, current
            0x00, 0x00, // section_number, last_section_number
            // PCR rides on the video PID: it is the stream that is always present.
            0xE0 | ((PID_VIDEO >> 8) as u8),
            (PID_VIDEO & 0xFF) as u8,
            0xF0,
            0x00, // program_info_length
        ];
        body.extend_from_slice(&streams);

        let mut section = vec![0x02];
        let length = body.len() + 4;
        section.push(0xB0 | ((length >> 8) as u8));
        section.push((length & 0xFF) as u8);
        section.extend_from_slice(&body);
        let crc = crc32(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        pmt.extend_from_slice(&section);
        emit(out, PID_PMT, &mut self.cc_pmt, &pmt, None, false);
    }

    /// One video access unit. `data` is Annex B as the encoder produced it.
    pub fn write_video(&mut self, out: &mut Vec<u8>, data: &[u8], pts_90k: u64, keyframe: bool) {
        if keyframe && self.sequence_header.is_none() {
            self.sequence_header = extract_sequence_header(data);
        }

        // A keyframe that carries no SPS is not independently decodable, which would make any
        // segment starting there useless. Re-insert rather than assume — this is per-vendor
        // behaviour, and the cost of checking is a scan of a few NAL headers.
        let owned;
        let payload = if keyframe && !has_sequence_header(data) {
            match &self.sequence_header {
                Some(header) => {
                    owned = [header.as_slice(), data].concat();
                    owned.as_slice()
                }
                None => data,
            }
        } else {
            data
        };

        let pes = build_pes(0xE0, payload, pts_90k, true);
        // PCR goes out with every keyframe, which is at least once a second — well inside the
        // 100 ms the spec asks for once a decoder has locked on, and enough for a player to start.
        emit(
            out,
            PID_VIDEO,
            &mut self.cc_video,
            &pes,
            if keyframe { Some(pts_90k) } else { None },
            keyframe,
        );
    }

    /// One ADTS AAC frame.
    pub fn write_audio(&mut self, out: &mut Vec<u8>, data: &[u8], pts_90k: u64) {
        let pes = build_pes(0xC0, data, pts_90k, false);
        emit(out, PID_AUDIO, &mut self.cc_audio, &pes, None, false);
    }
}

/// PES wrapper. Video uses an unbounded length (legal, and the access unit can exceed 65535);
/// audio must state its length.
fn build_pes(stream_id: u8, payload: &[u8], pts_90k: u64, unbounded: bool) -> Vec<u8> {
    let mut pes = Vec::with_capacity(payload.len() + 14);
    pes.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);

    // With B-frames disabled, DTS always equals PTS, so only PTS is carried. That is five bytes
    // saved per access unit and one less thing for the muxer to get wrong.
    let header_len = 5u8;
    let packet_len: u16 = if unbounded {
        0
    } else {
        (payload.len() + 3 + header_len as usize).min(0xFFFF) as u16
    };
    pes.extend_from_slice(&packet_len.to_be_bytes());
    pes.push(0x80); // '10', no scrambling, not priority
    pes.push(0x80); // PTS only
    pes.push(header_len);
    pes.extend_from_slice(&timestamp_field(0b0010, pts_90k));
    pes.extend_from_slice(payload);
    pes
}

/// The 5-byte PTS/DTS encoding, with its marker bits stitched between the value's fields.
fn timestamp_field(prefix: u8, value: u64) -> [u8; 5] {
    let v = value & 0x1_FFFF_FFFF; // 33 bits
    [
        (prefix << 4) | (((v >> 30) as u8) & 0x0E) | 0x01,
        ((v >> 22) & 0xFF) as u8,
        ((((v >> 14) as u8) & 0xFE)) | 0x01,
        ((v >> 7) & 0xFF) as u8,
        ((((v << 1) as u8) & 0xFE)) | 0x01,
    ]
}

fn pcr_field(pts_90k: u64) -> [u8; 6] {
    let base = pts_90k & 0x1_FFFF_FFFF;
    [
        ((base >> 25) & 0xFF) as u8,
        ((base >> 17) & 0xFF) as u8,
        ((base >> 9) & 0xFF) as u8,
        ((base >> 1) & 0xFF) as u8,
        (((base & 1) as u8) << 7) | 0x7E, // marker bits, extension high bit 0
        0x00,                             // extension low byte
    ]
}

/// Splits a payload across 188-byte transport packets, padding the last one with an adaptation
/// field so every packet is exactly full — TS has no notion of a short packet.
fn emit(
    out: &mut Vec<u8>,
    pid: u16,
    cc: &mut u8,
    payload: &[u8],
    pcr: Option<u64>,
    random_access: bool,
) {
    let mut offset = 0;
    let mut first = true;

    while first || offset < payload.len() {
        let mut af: Vec<u8> = Vec::new();
        if first && (pcr.is_some() || random_access) {
            let mut flags = 0u8;
            if random_access {
                flags |= 0x40;
            }
            if pcr.is_some() {
                flags |= 0x10;
            }
            af.push(flags);
            if let Some(pcr) = pcr {
                af.extend_from_slice(&pcr_field(pcr));
            }
        }

        let overhead = if af.is_empty() { 0 } else { af.len() + 1 };
        let space = PAYLOAD - overhead;
        let take = (payload.len() - offset).min(space);
        let slack = space - take;

        if slack > 0 {
            // Absorb the shortfall into the adaptation field rather than leaving the packet short.
            if af.is_empty() {
                // One byte of slack is exactly an adaptation_field_length of zero.
                af.resize(slack - 1, 0xFF);
            } else {
                af.resize(af.len() + slack, 0xFF);
            }
        }

        let has_af = !af.is_empty() || slack > 0;
        let afc = match (has_af, take > 0) {
            (true, true) => 0b11,
            (true, false) => 0b10,
            _ => 0b01,
        };

        out.push(0x47);
        out.push((if first { 0x40 } else { 0x00 }) | ((pid >> 8) as u8 & 0x1F));
        out.push((pid & 0xFF) as u8);
        out.push((afc << 4) | (*cc & 0x0F));
        *cc = cc.wrapping_add(1);

        if has_af {
            out.push(af.len() as u8);
            out.extend_from_slice(&af);
        }
        out.extend_from_slice(&payload[offset..offset + take]);

        offset += take;
        first = false;
    }
}

/// Walks Annex B start codes and reports the NAL unit types in order.
fn nal_types(data: &[u8]) -> Vec<(usize, u8)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            out.push((i, data[i + 3] & 0x1F));
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

fn has_sequence_header(data: &[u8]) -> bool {
    nal_types(data).iter().any(|&(_, t)| t == 7)
}

/// Everything from the first SPS up to (not including) the first slice, so the header can be
/// replayed verbatim in front of a later keyframe that lacks one.
fn extract_sequence_header(data: &[u8]) -> Option<Vec<u8>> {
    let nals = nal_types(data);
    let start = nals.iter().find(|&&(_, t)| t == 7)?.0;
    let end = nals
        .iter()
        .find(|&&(pos, t)| pos > start && (t == 1 || t == 5))
        .map(|&(pos, _)| pos)
        .unwrap_or(data.len());
    Some(data[start..end].to_vec())
}

/// MPEG-2 systems CRC: polynomial 0x04C11DB7, MSB first, no final inversion.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}
