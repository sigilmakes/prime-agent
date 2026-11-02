//! CRC-32 over the seq and a row's serialized bytes (the `zlib`/IEEE 802.3
//! checksum).

/// The IEEE polynomial, bit-reversed for the low-bit-first loop.
const POLYNOMIAL: u32 = 0xEDB8_8320;

const fn table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ POLYNOMIAL
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

static TABLE: [u32; 256] = table();

/// The CRC-32 of the parts, as if they were concatenated: lets a caller cover
/// the seq's bytes and the row's bytes without building a buffer.
#[must_use]
pub(crate) fn crc32_parts(parts: &[&[u8]]) -> u32 {
    let mut crc = u32::MAX;
    for part in parts {
        for &byte in *part {
            crc = TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::crc32_parts;

    #[test]
    fn matches_the_published_check_value_over_the_parts() {
        // The CRC-32 check value every implementation agrees on, reached both
        // whole and split: the parts hash like their concatenation.
        assert_eq!(crc32_parts(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32_parts(&[b"1234", b"56789"]), 0xCBF4_3926);
        assert_eq!(crc32_parts(&[b"", b"123456789", b""]), 0xCBF4_3926);
        assert_eq!(crc32_parts(&[b""]), 0);
    }
}
