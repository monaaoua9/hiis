use crate::error::{Error, Result};

// 7 payload bits per byte, high bit set means "more follows" (LEB128 order)
pub fn write_u64(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline]
pub fn write_u32(out: &mut Vec<u8>, v: u32) {
    write_u64(out, v as u64);
}

pub fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*pos).ok_or(Error::Corrupt("vbyte ran past end of buffer"))?;
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(Error::Corrupt("vbyte longer than 10 bytes"));
        }
    }
}

#[inline]
pub fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let v = read_u64(buf, pos)?;
    u32::try_from(v).map_err(|_| Error::Corrupt("vbyte value exceeds u32"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_edges() {
        let vals = [0u64, 1, 127, 128, 16_383, 16_384, u32::MAX as u64, u64::MAX];
        let mut buf = Vec::new();
        for &v in &vals {
            write_u64(&mut buf, v);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(read_u64(&buf, &mut pos).unwrap(), v);
        }
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn small_values_take_one_byte() {
        let mut buf = Vec::new();
        write_u32(&mut buf, 127);
        assert_eq!(buf.len(), 1);
        write_u32(&mut buf, 128);
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn truncated_input_is_an_error() {
        let mut pos = 0;
        assert!(read_u64(&[0x80, 0x80], &mut pos).is_err());
        let mut pos = 0;
        assert!(read_u64(&[0xff; 11], &mut pos).is_err());
    }

    #[test]
    fn u32_overflow_rejected() {
        let mut buf = Vec::new();
        write_u64(&mut buf, u32::MAX as u64 + 1);
        assert!(read_u32(&buf, &mut 0).is_err());
    }
}
