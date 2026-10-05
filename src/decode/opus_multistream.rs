//! Multistream (surround) Opus, RFC 7845 §5.1.1 + RFC 6716 Appendix B.
//!
//! A multistream packet holds one Opus packet per stream. All but the last use
//! "self-delimiting" framing: an extra length field right after the TOC byte (and the frame-count
//! byte for code 3). We undo that framing and decode each stream with a single-stream decoder.

use opus_pure::OpusDecoder as Mono;

/// Longest Opus packet: 120 ms at 48 kHz.
const MAX_FRAMES: usize = 5_760;

fn read_len(p: &[u8], pos: &mut usize) -> Result<usize, String> {
    let b0 = *p.get(*pos).ok_or("opus: truncated length")? as usize;
    *pos += 1;
    if b0 < 252 {
        return Ok(b0);
    }
    let b1 = *p.get(*pos).ok_or("opus: truncated length")? as usize;
    *pos += 1;
    Ok(b0 + 4 * b1)
}

fn take<'a>(p: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], String> {
    let bytes = p.get(*pos..*pos + n).ok_or("opus: sub-packet longer than the packet")?;
    *pos += n;
    Ok(bytes)
}

/// Converts the self-delimited packet at the start of `p` into an ordinary packet.
/// Returns the packet and how many bytes of `p` it used.
fn undelimit(p: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let toc = *p.first().ok_or("opus: empty sub-packet")?;
    let mut pos = 1;
    let mut out = vec![toc];
    match toc & 0b11 {
        0 => {
            let n = read_len(p, &mut pos)?;
            out.extend_from_slice(take(p, &mut pos, n)?);
        }
        1 => {
            let n = read_len(p, &mut pos)?;
            out.extend_from_slice(take(p, &mut pos, 2 * n)?);
        }
        2 => {
            let start = pos;
            let n1 = read_len(p, &mut pos)?;
            out.extend_from_slice(&p[start..pos]); // the first frame's length stays
            let n2 = read_len(p, &mut pos)?; // the self-delimiting one goes
            out.extend_from_slice(take(p, &mut pos, n1 + n2)?);
        }
        _ => {
            let count = *p.get(pos).ok_or("opus: missing frame count")?;
            pos += 1;
            out.push(count);
            let frames = (count & 0x3F) as usize;
            if frames == 0 {
                return Err("opus: code 3 packet with zero frames".into());
            }
            let mut padding = 0;
            if count & 0x40 != 0 {
                loop {
                    let b = *p.get(pos).ok_or("opus: truncated padding length")?;
                    pos += 1;
                    out.push(b);
                    padding += if b == 255 { 254 } else { b as usize };
                    if b != 255 {
                        break;
                    }
                }
            }
            let data = if count & 0x80 != 0 {
                // VBR: M-1 ordinary lengths, then the self-delimiting length of the last frame.
                let mut total = 0;
                for _ in 0..frames - 1 {
                    let start = pos;
                    total += read_len(p, &mut pos)?;
                    out.extend_from_slice(&p[start..pos]);
                }
                total + read_len(p, &mut pos)?
            } else {
                // CBR: the self-delimiting length is the size of every frame.
                frames * read_len(p, &mut pos)?
            };
            out.extend_from_slice(take(p, &mut pos, data)?);
            out.extend_from_slice(take(p, &mut pos, padding)?);
        }
    }
    Ok((out, pos))
}

/// Splits a multistream packet into one ordinary Opus packet per stream.
pub(crate) fn split_multistream(packet: &[u8], streams: usize) -> Result<Vec<Vec<u8>>, String> {
    if packet.is_empty() || streams == 0 {
        return Err("opus: empty multistream packet".into());
    }
    let mut pos = 0;
    let mut out = Vec::with_capacity(streams);
    for _ in 0..streams - 1 {
        let (sub, used) = undelimit(&packet[pos..])?;
        pos += used;
        out.push(sub);
    }
    let last = &packet[pos..];
    if last.is_empty() {
        return Err("opus: missing last stream".into());
    }
    out.push(last.to_vec());
    Ok(out)
}

/// The OpusHead channel mapping table.
pub(crate) struct Mapping {
    pub coupled: usize,
    /// One entry per output channel: a decoded-channel slot, or 255 for silence.
    pub mapping: Vec<u8>,
}

impl Mapping {
    /// (stream, channel within that stream) feeding output channel `out`.
    fn source(&self, out: usize) -> Option<(usize, usize)> {
        let slot = *self.mapping.get(out)? as usize;
        if slot == 255 {
            None
        } else if slot < 2 * self.coupled {
            Some((slot / 2, slot % 2))
        } else {
            Some((self.coupled + slot - 2 * self.coupled, 0))
        }
    }
}

pub(crate) struct MultistreamDecoder {
    decoders: Vec<Mono>,
    map: Mapping,
    channels: usize,
    bufs: Vec<Vec<f32>>,
}

impl MultistreamDecoder {
    pub fn new(rate: u32, channels: usize, streams: usize, coupled: usize, mapping: Vec<u8>) -> Result<Self, String> {
        if streams == 0 || coupled > streams || mapping.len() != channels {
            return Err(format!("opus: bad channel mapping ({streams} streams, {coupled} coupled, {channels} channels)"));
        }
        if mapping.iter().any(|&m| m != 255 && m as usize >= streams + coupled) {
            return Err("opus: channel mapping refers to a missing stream".into());
        }
        let decoders = (0..streams)
            .map(|i| Mono::new(rate as i32, if i < coupled { 2 } else { 1 }).map_err(|e| format!("opus init: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        let bufs = (0..streams).map(|i| vec![0.0; MAX_FRAMES * if i < coupled { 2 } else { 1 }]).collect();
        Ok(Self { decoders, map: Mapping { coupled, mapping }, channels, bufs })
    }

    /// Decodes into interleaved `pcm` (`channels` per frame, in OpusHead order). Returns frames.
    pub fn decode_float(&mut self, packet: &[u8], pcm: &mut [f32]) -> Result<usize, String> {
        let subs = split_multistream(packet, self.decoders.len())?;
        let mut frames = None;
        for ((dec, buf), sub) in self.decoders.iter_mut().zip(&mut self.bufs).zip(&subs) {
            let n = dec.decode(sub, MAX_FRAMES, buf).map_err(|e| format!("opus: {e}"))?;
            if *frames.get_or_insert(n) != n {
                return Err("opus: streams decoded different frame counts".into());
            }
        }
        let frames = frames.unwrap_or(0);
        if pcm.len() < frames * self.channels {
            return Err("opus: output buffer too small".into());
        }
        for f in 0..frames {
            for c in 0..self.channels {
                pcm[f * self.channels + c] = match self.map.source(c) {
                    Some((s, sc)) => {
                        let stride = if s < self.map.coupled { 2 } else { 1 };
                        self.bufs[s][f * stride + sc]
                    }
                    None => 0.0,
                };
            }
        }
        Ok(frames)
    }

    pub fn reset(&mut self) {
        for d in &mut self.decoders {
            let _ = d.reset_state();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Last stream: an ordinary (undelimited) packet, passed through untouched.
    const LAST: [u8; 3] = [0x08, 0xAA, 0xBB];

    fn split2(first: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let mut p = first.to_vec();
        p.extend(LAST);
        split_multistream(&p, 2)
    }

    #[test]
    fn code0_drops_the_self_delimiting_length() {
        let out = split2(&[0x00, 3, 1, 2, 3]).unwrap();
        assert_eq!(out, vec![vec![0x00, 1, 2, 3], LAST.to_vec()]);
    }

    #[test]
    fn two_byte_lengths() {
        // 300 = 252 + 4 * 12
        let mut first = vec![0x00, 252, 12];
        first.extend(std::iter::repeat_n(7u8, 300));
        let out = split2(&first).unwrap();
        assert_eq!(out[0].len(), 301);
        assert_eq!(out[1], LAST);
    }

    #[test]
    fn code1_two_equal_frames() {
        let out = split2(&[0x01, 2, 1, 2, 3, 4]).unwrap();
        assert_eq!(out[0], vec![0x01, 1, 2, 3, 4]);
    }

    #[test]
    fn code2_keeps_first_length_drops_second() {
        // TOC, N1 = 1, self-delimiting N2 = 2, frame1 (1 byte), frame2 (2 bytes)
        let out = split2(&[0x02, 1, 2, 9, 8, 8]).unwrap();
        assert_eq!(out[0], vec![0x02, 1, 9, 8, 8]);
    }

    #[test]
    fn code3_vbr_with_padding() {
        // count byte: VBR (0x80) | padding (0x40) | M = 3; 2 bytes of padding;
        // N1 = 1, N2 = 2, self-delimiting N3 = 1; frames 1 + 2 + 1 bytes; 2 padding bytes.
        let first = [0x03, 0xC3, 2, 1, 2, 1, 5, 6, 6, 7, 0, 0];
        let out = split2(&first).unwrap();
        assert_eq!(out[0], vec![0x03, 0xC3, 2, 1, 2, 5, 6, 6, 7, 0, 0]);
        assert_eq!(out[1], LAST);
    }

    #[test]
    fn code3_cbr_length_applies_to_every_frame() {
        // M = 2 CBR frames of 3 bytes each; the self-delimiting length gives the frame size.
        let out = split2(&[0x03, 0x02, 3, 1, 1, 1, 2, 2, 2]).unwrap();
        assert_eq!(out[0], vec![0x03, 0x02, 1, 1, 1, 2, 2, 2]);
    }

    #[test]
    fn truncated_or_inconsistent_packets_are_errors() {
        assert!(split_multistream(&[0x00, 5, 1, 2], 2).is_err(), "frame longer than the packet");
        assert!(split_multistream(&[], 2).is_err());
        assert!(split_multistream(&[0x03], 2).is_err(), "missing frame count");
        assert!(split_multistream(&[0x03, 0x00, 1, 1], 2).is_err(), "zero frames");
    }

    #[test]
    fn single_stream_is_the_packet_itself() {
        assert_eq!(split_multistream(&LAST, 1).unwrap(), vec![LAST.to_vec()]);
    }

    #[test]
    fn maps_output_channels_to_streams() {
        // 2 streams, 1 coupled: slots 0,1 -> stream 0 (L, R), slot 2 -> stream 1 (mono), 255 silent.
        let m = Mapping { coupled: 1, mapping: vec![0, 2, 1, 255] };
        assert_eq!(m.source(0), Some((0, 0)));
        assert_eq!(m.source(1), Some((1, 0)));
        assert_eq!(m.source(2), Some((0, 1)));
        assert_eq!(m.source(3), None);
    }
}
