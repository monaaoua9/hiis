//! Frame-of-reference packing for fixed-size blocks.
//!
//! One packed run is `[width: u8][min: vbyte][ceil(n*width/8) bytes]`. Every
//! value is stored as `v - min` in `width` bits, LSB first. When all values
//! are equal the width is 0 and no payload follows, which is the usual case
//! for the term-frequency block of a rare term (all 1s).

use super::vbyte;
use crate::error::{Error, Result};

pub const BLOCK_LEN: usize = 128;

pub fn pack(vals: &[u32], out: &mut Vec<u8>) {
    let min = vals.iter().copied().min().unwrap_or(0);
    let max = vals.iter().copied().max().unwrap_or(0);
    let width = 32 - (max - min).leading_zeros();

    out.push(width as u8);
    vbyte::write_u32(out, min);
    if width == 0 {
        return;
    }

    let mut acc = 0u64;
    let mut nbits = 0u32;
    for &v in vals {
        acc |= ((v - min) as u64) << nbits;
        nbits += width;
        while nbits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            nbits -= 8;
        }
    }
    if nbits > 0 {
        out.push(acc as u8);
    }
}

pub fn unpack(buf: &[u8], pos: &mut usize, n: usize, out: &mut Vec<u32>) -> Result<()> {
    let width = *buf.get(*pos).ok_or(Error::Corrupt("bitpack header missing"))? as u32;
    *pos += 1;
    if width > 32 {
        return Err(Error::Corrupt("bitpack width > 32"));
    }
    let min = vbyte::read_u32(buf, pos)?;

    if width == 0 {
        out.extend(std::iter::repeat_n(min, n));
        return Ok(());
    }

    let nbytes = (n * width as usize).div_ceil(8);
    let end = pos.checked_add(nbytes).ok_or(Error::Corrupt("bitpack length overflow"))?;
    let bytes = buf.get(*pos..end).ok_or(Error::Corrupt("bitpack payload truncated"))?;
    *pos = end;

    let mask = (1u64 << width) - 1;
    let mut it = bytes.iter();
    let mut acc = 0u64;
    let mut nbits = 0u32;
    for _ in 0..n {
        while nbits < width {
            acc |= (*it.next().unwrap_or(&0) as u64) << nbits;
            nbits += 8;
        }
        out.push(((acc & mask) as u32).wrapping_add(min));
        acc >>= width;
        nbits -= width;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(vals: &[u32]) -> usize {
        let mut buf = Vec::new();
        pack(vals, &mut buf);
        let mut out = Vec::new();
        let mut pos = 0;
        unpack(&buf, &mut pos, vals.len(), &mut out).unwrap();
        assert_eq!(out, vals);
        assert_eq!(pos, buf.len());
        buf.len()
    }

    #[test]
    fn all_equal_has_no_payload() {
        let size = roundtrip(&[1; BLOCK_LEN]);
        assert_eq!(size, 2); // width + min
    }

    #[test]
    fn every_width() {
        for w in 1..=32u32 {
            let top = if w == 32 { u32::MAX } else { (1u32 << w) - 1 };
            let vals: Vec<u32> = (0..BLOCK_LEN as u32).map(|i| top - (i * 7 % 13).min(top)).collect();
            roundtrip(&vals);
        }
    }

    #[test]
    fn nonzero_min() {
        let vals: Vec<u32> = (0..BLOCK_LEN as u32).map(|i| 1_000_000 + i % 5).collect();
        let size = roundtrip(&vals);
        assert!(size < 70, "FoR should need ~3 bits/value, got {size} bytes");
    }

    #[test]
    fn truncated_payload_is_an_error() {
        let vals: Vec<u32> = (0..BLOCK_LEN as u32).collect();
        let mut buf = Vec::new();
        pack(&vals, &mut buf);
        buf.truncate(buf.len() - 4);
        assert!(unpack(&buf, &mut 0, BLOCK_LEN, &mut Vec::new()).is_err());
    }
}
