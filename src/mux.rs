//! Minimal MP4 writer for H.264 + AAC-LC. Writes the moov box before mdat
//! ("faststart") so clips start playing immediately when uploaded somewhere.

use crate::prelude::*;

use crate::rt::fs::{BufWriter, File};
use crate::rt::{self, error};

use crate::clock::SECOND;
use crate::ring::Clip;

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
pub const AUDIO_BITRATE: u32 = 192_000;
const AAC_FRAME: u32 = 1024;

const MOVIE_TIMESCALE: u32 = 1000;
const VIDEO_TIMESCALE: u32 = 90_000;

// ---------------------------------------------------------------------------
// Annex B helpers

/// Iterate NAL units in an Annex B byte stream (start codes stripped).
fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts.iter().skip(1).map(|&s| s - 3).chain([data.len()]).collect();
    starts.into_iter().zip(ends).map(move |(s, e)| {
        // A 4-byte start code leaves a zero at the end of the previous NAL.
        let mut e = e;
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        &data[s..e]
    })
}

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// NALs that belong in the sample data (parameter sets live in avcC, and
/// access unit delimiters are pointless in MP4).
fn keep_nal(nal: &[u8]) -> bool {
    !nal.is_empty() && !matches!(nal_type(nal), 7 | 8 | 9)
}

fn avcc_size(annexb: &[u8]) -> u32 {
    nal_units(annexb).filter(|n| keep_nal(n)).map(|n| 4 + n.len() as u32).sum()
}

fn write_avcc(annexb: &[u8], w: &mut BufWriter) -> rt::Result<()> {
    for nal in nal_units(annexb).filter(|n| keep_nal(n)) {
        w.write_all(&(nal.len() as u32).to_be_bytes())?;
        w.write_all(nal)?;
    }
    Ok(())
}

fn find_param_sets(clip: &Clip) -> Option<(Vec<u8>, Vec<u8>)> {
    let sources =
        clip.video.iter().filter(|p| p.key).map(|p| p.data.as_slice()).chain([clip.format.seq_header.as_slice()]);
    for data in sources {
        let mut sps = None;
        let mut pps = None;
        for nal in nal_units(data) {
            match nal_type(nal) {
                7 if sps.is_none() => sps = Some(nal.to_vec()),
                8 if pps.is_none() => pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        if let (Some(s), Some(p)) = (sps, pps) {
            return Some((s, p));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Box building

struct Mp4 {
    buf: Vec<u8>,
    stack: Vec<usize>,
}

impl Mp4 {
    fn new() -> Self {
        Self { buf: Vec::new(), stack: Vec::new() }
    }
    fn begin(&mut self, kind: &[u8; 4]) {
        self.stack.push(self.buf.len());
        self.u32(0);
        self.buf.extend_from_slice(kind);
    }
    fn begin_full(&mut self, kind: &[u8; 4], version: u8, flags: u32) {
        self.begin(kind);
        self.u32((version as u32) << 24 | flags);
    }
    fn end(&mut self) {
        let start = self.stack.pop().unwrap();
        let size = (self.buf.len() - start) as u32;
        self.buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }
    fn matrix(&mut self) {
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            self.u32(v);
        }
    }
}

struct Track {
    id: u32,
    timescale: u32,
    sizes: Vec<u32>,
    offsets: Vec<u64>,
    /// Run-length encoded (count, delta) sample durations.
    stts: Vec<(u32, u32)>,
    /// Run-length encoded (count, offset) composition offsets; empty when
    /// frames are never reordered.
    ctts: Vec<(u32, i32)>,
    /// 1-based sample numbers of sync samples (video only).
    sync: Vec<u32>,
    duration: u64,
    /// Delay before the first sample, in movie timescale.
    start_offset: u64,
}

impl Track {
    fn push_duration(&mut self, d: u32) {
        match self.stts.last_mut() {
            Some((count, delta)) if *delta == d => *count += 1,
            _ => self.stts.push((1, d)),
        }
        self.duration += d as u64;
    }

    fn movie_duration(&self) -> u32 {
        (self.duration * MOVIE_TIMESCALE as u64 / self.timescale as u64) as u32
    }

    fn write_stbl_tables(&self, m: &mut Mp4) {
        m.begin_full(b"stts", 0, 0);
        m.u32(self.stts.len() as u32);
        for &(count, delta) in &self.stts {
            m.u32(count);
            m.u32(delta);
        }
        m.end();

        if !self.ctts.is_empty() {
            m.begin_full(b"ctts", 1, 0); // version 1: signed offsets
            m.u32(self.ctts.len() as u32);
            for &(count, offset) in &self.ctts {
                m.u32(count);
                m.u32(offset as u32);
            }
            m.end();
        }

        if self.id == 1 {
            m.begin_full(b"stss", 0, 0);
            m.u32(self.sync.len() as u32);
            for &s in &self.sync {
                m.u32(s);
            }
            m.end();
        }

        // One sample per chunk keeps the bookkeeping trivial.
        m.begin_full(b"stsc", 0, 0);
        m.u32(1);
        m.u32(1);
        m.u32(1);
        m.u32(1);
        m.end();

        m.begin_full(b"stsz", 0, 0);
        m.u32(0);
        m.u32(self.sizes.len() as u32);
        for &s in &self.sizes {
            m.u32(s);
        }
        m.end();

        m.begin_full(b"co64", 0, 0);
        m.u32(self.offsets.len() as u32);
        for &o in &self.offsets {
            m.u64(o);
        }
        m.end();
    }

    fn write_header(&self, m: &mut Mp4, width: u32, height: u32) {
        m.begin_full(b"tkhd", 0, 3); // enabled | in movie
        m.u32(0);
        m.u32(0);
        m.u32(self.id);
        m.u32(0);
        m.u32(self.movie_duration() + self.start_offset as u32);
        m.zeros(8);
        m.u16(0); // layer
        m.u16(0); // alternate group
        m.u16(if self.id == 2 { 0x0100 } else { 0 }); // volume
        m.u16(0);
        m.matrix();
        m.u32(width << 16);
        m.u32(height << 16);
        m.end();

        if self.start_offset > 0 {
            m.begin(b"edts");
            m.begin_full(b"elst", 0, 0);
            m.u32(2);
            m.u32(self.start_offset as u32); // empty edit
            m.u32(u32::MAX); // media_time = -1
            m.u32(0x0001_0000);
            m.u32(self.movie_duration());
            m.u32(0);
            m.u32(0x0001_0000);
            m.end();
            m.end();
        }
    }

    fn write_mdhd(&self, m: &mut Mp4, handler: &[u8; 4], name: &str) {
        m.begin_full(b"mdhd", 0, 0);
        m.u32(0);
        m.u32(0);
        m.u32(self.timescale);
        m.u32(self.duration as u32);
        m.u16(0x55c4); // "und"
        m.u16(0);
        m.end();

        m.begin_full(b"hdlr", 0, 0);
        m.u32(0);
        m.bytes(handler);
        m.zeros(12);
        m.bytes(name.as_bytes());
        m.u8(0);
        m.end();
    }
}

fn write_dinf(m: &mut Mp4) {
    m.begin(b"dinf");
    m.begin_full(b"dref", 0, 0);
    m.u32(1);
    m.begin_full(b"url ", 0, 1);
    m.end();
    m.end();
    m.end();
}

fn write_moov(clip: &Clip, sps: &[u8], pps: &[u8], video: &Track, audio: Option<&Track>) -> Vec<u8> {
    let f = &clip.format;
    let mut m = Mp4::new();
    m.begin(b"moov");

    let movie_duration =
        audio.map(|a| a.movie_duration() + a.start_offset as u32).unwrap_or(0).max(video.movie_duration());
    m.begin_full(b"mvhd", 0, 0);
    m.u32(0);
    m.u32(0);
    m.u32(MOVIE_TIMESCALE);
    m.u32(movie_duration);
    m.u32(0x0001_0000); // rate
    m.u16(0x0100); // volume
    m.zeros(10);
    m.matrix();
    m.zeros(24);
    m.u32(3); // next track id
    m.end();

    // --- video track
    m.begin(b"trak");
    video.write_header(&mut m, f.width, f.height);
    m.begin(b"mdia");
    video.write_mdhd(&mut m, b"vide", "VideoHandler");
    m.begin(b"minf");
    m.begin_full(b"vmhd", 0, 1);
    m.zeros(8);
    m.end();
    write_dinf(&mut m);
    m.begin(b"stbl");
    m.begin_full(b"stsd", 0, 0);
    m.u32(1);
    m.begin(b"avc1");
    m.zeros(6);
    m.u16(1); // data reference index
    m.zeros(16);
    m.u16(f.width as u16);
    m.u16(f.height as u16);
    m.u32(0x0048_0000);
    m.u32(0x0048_0000);
    m.u32(0);
    m.u16(1); // frame count
    let mut compressor = [0u8; 32];
    compressor[0] = 10;
    compressor[1..11].copy_from_slice(b"bufferless");
    m.bytes(&compressor);
    m.u16(0x0018);
    m.u16(0xffff);
    m.begin(b"avcC");
    m.u8(1);
    m.u8(sps[1]);
    m.u8(sps[2]);
    m.u8(sps[3]);
    m.u8(0xff); // 4-byte NAL lengths
    m.u8(0xe1); // 1 SPS
    m.u16(sps.len() as u16);
    m.bytes(sps);
    m.u8(1);
    m.u16(pps.len() as u16);
    m.bytes(pps);
    m.end();
    // BT.709 limited range, matching what the video processor outputs.
    m.begin(b"colr");
    m.bytes(b"nclx");
    m.u16(1);
    m.u16(1);
    m.u16(1);
    m.u8(0);
    m.end();
    m.end(); // avc1
    m.end(); // stsd
    video.write_stbl_tables(&mut m);
    m.end(); // stbl
    m.end(); // minf
    m.end(); // mdia
    m.end(); // trak

    // --- audio track
    if let Some(audio) = audio {
        m.begin(b"trak");
        audio.write_header(&mut m, 0, 0);
        m.begin(b"mdia");
        audio.write_mdhd(&mut m, b"soun", "SoundHandler");
        m.begin(b"minf");
        m.begin_full(b"smhd", 0, 0);
        m.u32(0);
        m.end();
        write_dinf(&mut m);
        m.begin(b"stbl");
        m.begin_full(b"stsd", 0, 0);
        m.u32(1);
        m.begin(b"mp4a");
        m.zeros(6);
        m.u16(1);
        m.zeros(8);
        m.u16(AUDIO_CHANNELS);
        m.u16(16);
        m.u32(0);
        m.u32(AUDIO_RATE << 16);
        m.begin_full(b"esds", 0, 0);
        // ES_Descriptor
        m.u8(0x03);
        m.u8(25);
        m.u16(2); // ES_ID
        m.u8(0);
        // DecoderConfigDescriptor
        m.u8(0x04);
        m.u8(17);
        m.u8(0x40); // MPEG-4 audio
        m.u8(0x15); // audio stream
        m.u8(0);
        m.u16(0); // buffer size
        m.u32(AUDIO_BITRATE);
        m.u32(AUDIO_BITRATE);
        // DecoderSpecificInfo: AudioSpecificConfig for AAC-LC
        m.u8(0x05);
        m.u8(2);
        let freq_index: u16 = match AUDIO_RATE {
            44_100 => 4,
            _ => 3, // 48 kHz
        };
        m.u16(2 << 11 | freq_index << 7 | (AUDIO_CHANNELS << 3));
        // SLConfigDescriptor
        m.u8(0x06);
        m.u8(1);
        m.u8(0x02);
        m.end(); // esds
        m.end(); // mp4a
        m.end(); // stsd
        audio.write_stbl_tables(&mut m);
        m.end();
        m.end();
        m.end();
        m.end();
    }

    m.end(); // moov
    m.buf
}

/// Convert a timestamp relative to the clip start into track timescale units.
fn to_scale(t: i64, scale: u32) -> u64 {
    (t.max(0) as i128 * scale as i128 / SECOND as i128) as u64
}

pub fn write_mp4(path: &str, clip: &Clip) -> rt::Result<u64> {
    let invalid = error;
    let (sps, pps) = find_param_sets(clip).ok_or_else(|| invalid("no SPS/PPS in stream"))?;
    if sps.len() < 4 {
        return Err(invalid("bad SPS"));
    }
    let t0 = clip.video.iter().map(|p| p.pts).min().ok_or_else(|| invalid("empty clip"))?;

    let new_track = |id, timescale| Track {
        id,
        timescale,
        sizes: Vec::new(),
        offsets: Vec::new(),
        stts: Vec::new(),
        ctts: Vec::new(),
        sync: Vec::new(),
        duration: 0,
        start_offset: 0,
    };

    // Video sample table. Samples are stored in decode order; decode times are
    // the sorted presentation times, which also covers encoders that emit
    // B-frames (their reordering goes into ctts). Durations come from timestamp
    // deltas so dropped frames keep the clip in sync with audio.
    let mut video = new_track(1, VIDEO_TIMESCALE);
    let pts: Vec<i64> = clip.video.iter().map(|p| to_scale(p.pts - t0, VIDEO_TIMESCALE) as i64).collect();
    let mut dts = pts.clone();
    dts.sort_unstable();
    let nominal = VIDEO_TIMESCALE / clip.format.fps.max(1);
    for (i, p) in clip.video.iter().enumerate() {
        video.sizes.push(avcc_size(&p.data));
        if p.key {
            video.sync.push(i as u32 + 1);
        }
        let d = dts.get(i + 1).map_or(nominal, |next| (next - dts[i]) as u32);
        video.push_duration(d.max(1));
    }
    if pts != dts {
        for (p, d) in pts.iter().zip(&dts) {
            let offset = (p - d) as i32;
            match video.ctts.last_mut() {
                Some((count, o)) if *o == offset => *count += 1,
                _ => video.ctts.push((1, offset)),
            }
        }
    }

    let mut audio = if clip.audio.is_empty() {
        None
    } else {
        let mut a = new_track(2, AUDIO_RATE);
        a.start_offset = to_scale(clip.audio[0].pts - t0, MOVIE_TIMESCALE);
        for p in &clip.audio {
            a.sizes.push(p.data.len() as u32);
            a.push_duration(AAC_FRAME);
        }
        Some(a)
    };

    // Interleave samples by timestamp: (track, index).
    let mut order: Vec<(bool, usize, i64)> = Vec::with_capacity(clip.video.len() + clip.audio.len());
    order.extend(clip.video.iter().enumerate().map(|(i, p)| (true, i, p.pts)));
    order.extend(clip.audio.iter().enumerate().map(|(i, p)| (false, i, p.pts)));
    order.sort_by_key(|&(_, _, pts)| pts);

    let assign_offsets = |base: u64, video: &mut Track, audio: &mut Option<Track>| {
        video.offsets.clear();
        if let Some(a) = audio.as_mut() {
            a.offsets.clear();
        }
        let mut off = base;
        for &(is_video, i, _) in &order {
            let track = if is_video { &mut *video } else { audio.as_mut().unwrap() };
            track.offsets.push(off);
            off += track.sizes[i] as u64;
        }
        off - base
    };

    let mut ftyp = Mp4::new();
    ftyp.begin(b"ftyp");
    ftyp.bytes(b"isom");
    ftyp.u32(0x200);
    ftyp.bytes(b"isomiso2avc1mp41");
    ftyp.end();

    // co64 entries are fixed size, so the moov size doesn't depend on the
    // offsets: build once to measure, then again with real offsets.
    let mdat_payload = assign_offsets(0, &mut video, &mut audio);
    let moov_len = write_moov(clip, &sps, &pps, &video, audio.as_ref()).len() as u64;
    let mdat_header = 16u64;
    let base = ftyp.buf.len() as u64 + moov_len + mdat_header;
    assign_offsets(base, &mut video, &mut audio);
    let moov = write_moov(clip, &sps, &pps, &video, audio.as_ref());
    debug_assert_eq!(moov.len() as u64, moov_len);

    let mut w = BufWriter::new(File::create(path)?, 1 << 20);
    w.write_all(&ftyp.buf)?;
    w.write_all(&moov)?;
    w.write_all(&1u32.to_be_bytes())?;
    w.write_all(b"mdat")?;
    w.write_all(&(mdat_header + mdat_payload).to_be_bytes())?;
    for &(is_video, i, _) in &order {
        if is_video {
            write_avcc(&clip.video[i].data, &mut w)?;
        } else {
            w.write_all(&clip.audio[i].data)?;
        }
    }
    w.flush()?;
    Ok(base + mdat_payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annexb() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
        assert_eq!(avcc_size(&data), 4 + 3);
    }
}
