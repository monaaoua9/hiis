//! Byte-oriented LZ77 for the stored-text blocks. Same sequence layout as LZ4
//! (token, literals, 16-bit offset, extra length bytes) but nothing here has to
//! interoperate with it; the decoder knows the output size up front and checks
//! every length and offset against it, so a corrupt block is an error.

use crate::error::{Error, Result};

const MIN_MATCH: usize = 4;
const HASH_BITS: u32 = 14;
const MAX_OFFSET: usize = u16::MAX as usize;

fn hash(v: u32) -> usize {
    (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

fn read4(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn put_len(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

fn emit(out: &mut Vec<u8>, lits: &[u8], m: Option<(usize, usize)>) {
    let ml = m.map_or(0, |(_, len)| len - MIN_MATCH);
    out.push((lits.len().min(15) << 4) as u8 | ml.min(15) as u8);
    if lits.len() >= 15 {
        put_len(out, lits.len() - 15);
    }
    out.extend_from_slice(lits);
    if let Some((off, _)) = m {
        out.extend_from_slice(&(off as u16).to_le_bytes());
        if ml >= 15 {
            put_len(out, ml - 15);
        }
    }
}

pub fn compress(src: &[u8], out: &mut Vec<u8>) {
    let mut table = vec![0u32; 1 << HASH_BITS]; // position + 1, 0 = empty
    let (mut i, mut anchor) = (0, 0);

    while i + MIN_MATCH <= src.len() {
        let h = hash(read4(src, i));
        let cand = table[h] as usize;
        table[h] = (i + 1) as u32;

        if cand > 0 {
            let c = cand - 1;
            if i - c <= MAX_OFFSET && read4(src, c) == read4(src, i) {
                let mut m = MIN_MATCH;
                while i + m < src.len() && src[c + m] == src[i + m] {
                    m += 1;
                }
                emit(out, &src[anchor..i], Some((i - c, m)));
                i += m;
                anchor = i;
                continue;
            }
        }
        i += 1;
    }
    // trailing literals; the decoder stops when the input runs out here
    emit(out, &src[anchor..], None);
}

fn read_len(src: &[u8], p: &mut usize, base: usize) -> Result<usize> {
    let mut n = base;
    if base == 15 {
        loop {
            let b = *src.get(*p).ok_or(Error::Corrupt("lz length truncated"))?;
            *p += 1;
            n = n.checked_add(b as usize).ok_or(Error::Corrupt("lz length overflow"))?;
            if b != 255 {
                break;
            }
        }
    }
    Ok(n)
}

pub fn decompress(src: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(expected);
    let mut p = 0;

    loop {
        let token = *src.get(p).ok_or(Error::Corrupt("lz block truncated"))?;
        p += 1;
        let lit = read_len(src, &mut p, (token >> 4) as usize)?;
        let lits =
            p.checked_add(lit).and_then(|e| src.get(p..e)).ok_or(Error::Corrupt("lz literals truncated"))?;
        if out.len() + lit > expected {
            return Err(Error::Corrupt("lz output longer than expected"));
        }
        out.extend_from_slice(lits);
        p += lit;
        if p == src.len() {
            break;
        }

        let off = src.get(p..p + 2).ok_or(Error::Corrupt("lz offset truncated"))?;
        let off = u16::from_le_bytes([off[0], off[1]]) as usize;
        p += 2;
        let m = read_len(src, &mut p, (token & 15) as usize)? + MIN_MATCH;
        if off == 0 || off > out.len() || out.len() + m > expected {
            return Err(Error::Corrupt("lz match out of range"));
        }
        // overlapping copies (off < m) repeat the pattern, so go byte by byte
        let start = out.len() - off;
        if off >= m {
            out.extend_from_within(start..start + m);
        } else {
            for k in 0..m {
                out.push(out[start + k]);
            }
        }
    }

    if out.len() != expected {
        return Err(Error::Corrupt("lz output shorter than expected"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) -> usize {
        let mut c = Vec::new();
        compress(data, &mut c);
        assert_eq!(decompress(&c, data.len()).unwrap(), data);
        c.len()
    }

    #[test]
    fn edge_sizes() {
        for n in [0usize, 1, 3, 4, 5, 15, 16, 255, 256, 300, 70_000] {
            roundtrip(&vec![b'a'; n]);
            let mixed: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
            roundtrip(&mixed);
        }
    }

    #[test]
    fn repetitive_text_shrinks() {
        let text = "the quick brown fox jumps over the lazy dog. ".repeat(200);
        let c = roundtrip(text.as_bytes());
        assert!(c < text.len() / 5, "{c} of {}", text.len());
    }

    #[test]
    fn random_data_survives() {
        let mut x = 0x1234_5678_9abc_def0u64;
        let data: Vec<u8> = (0..50_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                // few distinct bytes so matches do occur
                (x % 7) as u8
            })
            .collect();
        roundtrip(&data);
    }

    #[test]
    fn garbage_never_panics() {
        let mut x = 99u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..3000 {
            let n = (next() % 64) as usize;
            let junk: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            let _ = decompress(&junk, (next() % 200) as usize);
        }
        // wrong expected size on a valid block is an error, not a panic
        let mut c = Vec::new();
        compress(b"hello hello hello hello", &mut c);
        assert!(decompress(&c, 5).is_err());
        assert!(decompress(&c, 500).is_err());
    }
}
