const POLY: u32 = 0xEDB8_8320;

const TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Incremental form, for files too big to hold in one slice.
pub struct Crc(u32);

impl Crc {
    pub fn new() -> Self {
        Crc(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

impl Default for Crc {
    fn default() -> Self {
        Self::new()
    }
}

pub fn checksum(data: &[u8]) -> u32 {
    let mut c = Crc::new();
    c.update(data);
    c.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_vector() {
        assert_eq!(super::checksum(b"123456789"), 0xCBF4_3926);
        assert_eq!(super::checksum(b""), 0);
        let mut c = super::Crc::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.finish(), 0xCBF4_3926);
    }
}
