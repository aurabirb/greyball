use std::io::{Read, Seek, SeekFrom};

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn read_at<R: Read + Seek>(r: &mut R, pos: u64, len: usize) -> Option<Vec<u8>> {
    r.seek(SeekFrom::Start(pos)).ok()?;
    let mut b = vec![0; len];
    r.read_exact(&mut b).ok()?;
    Some(b)
}

/// Fragmented MP4 (HLS) has `mdhd.duration == 0`, which makes symphonia report a zero-length
/// stream and rodio clamp every seek to 0. Returns the file offset of that field plus the
/// duration summed from the `sidx` boxes, in `mdhd` timescale units.
pub fn duration_patch<R: Read + Seek>(r: &mut R, total_len: u64) -> Option<(u64, [u8; 4])> {
    let mut mdhd: Option<(u64, u32)> = None;
    let mut sidx_ticks: Vec<(u32, u64)> = Vec::new();
    let mut pos = 0;
    while pos + 8 <= total_len {
        let head = read_at(r, pos, 8)?;
        let size = u32_at(&head, 0)? as u64;
        if size < 8 {
            break;
        }
        match &head[4..8] {
            b"moov" => {
                let body = read_at(r, pos + 8, (size - 8) as usize)?;
                let i = body.windows(4).position(|w| w == b"mdhd")?;
                if *body.get(i + 4)? != 0 || u32_at(&body, i + 20)? != 0 {
                    return None;
                }
                mdhd = Some((pos + 8 + i as u64 + 20, u32_at(&body, i + 16)?));
            }
            b"sidx" => {
                let b = read_at(r, pos + 8, (size - 8) as usize)?;
                let (mut o, ver) = (12, *b.first()?);
                let timescale = u32_at(&b, 8)?;
                o += if ver == 0 { 8 } else { 16 };
                let count = u16::from_be_bytes(b.get(o + 2..o + 4)?.try_into().ok()?) as usize;
                o += 4;
                let ticks = (0..count).map(|k| u32_at(&b, o + k * 12 + 4).map(u64::from)).sum::<Option<u64>>()?;
                sidx_ticks.push((timescale, ticks));
            }
            _ => {}
        }
        pos += size;
    }
    let (offset, mdhd_scale) = mdhd?;
    let total: u64 = sidx_ticks.iter().map(|&(ts, t)| t * mdhd_scale as u64 / ts.max(1) as u64).sum();
    if total == 0 {
        return None;
    }
    Some((offset, u32::try_from(total).ok()?.to_be_bytes()))
}
