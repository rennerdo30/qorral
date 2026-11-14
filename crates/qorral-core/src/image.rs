//! Guest screenshots: QEMU writes binary PPM (P6); this reads it.

/// An 8-bit RGB image.
#[derive(Clone, Debug, PartialEq)]
pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// Parses a binary PPM (`P6`, maxval ≤ 255). Bounded: refuses absurd sizes
/// and truncated data instead of allocating or panicking.
pub fn parse_ppm(data: &[u8]) -> Option<Rgb> {
    let mut pos = 0usize;
    let mut field = || -> Option<&[u8]> {
        loop {
            while pos < data.len() && data[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos < data.len() && data[pos] == b'#' {
                while pos < data.len() && data[pos] != b'\n' {
                    pos += 1;
                }
                continue;
            }
            break;
        }
        let start = pos;
        while pos < data.len() && !data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        (pos > start).then(|| &data[start..pos])
    };
    if field()? != b"P6" {
        return None;
    }
    let num = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<usize>().ok();
    let w = num(field()?)?;
    let h = num(field()?)?;
    let max = num(field()?)?;
    if w == 0 || h == 0 || w > 16384 || h > 16384 || max == 0 || max > 255 {
        return None;
    }
    // Exactly one whitespace byte separates the header from the pixels.
    let start = pos + 1;
    let len = w * h * 3;
    let px = data.get(start..start + len)?.to_vec();
    Some(Rgb { w, h, px })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_p6_with_comments_and_rejects_bad_input() {
        let mut d = b"P6\n# qemu\n2 1\n255\n".to_vec();
        d.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(parse_ppm(&d), Some(Rgb { w: 2, h: 1, px: vec![1, 2, 3, 4, 5, 6] }));
        assert_eq!(parse_ppm(&d[..d.len() - 1]), None, "truncated");
        assert_eq!(parse_ppm(b"P3\n1 1\n255\n0 0 0"), None, "ASCII PPM is not what QEMU writes");
        assert_eq!(parse_ppm(b"P6\n99999 99999\n255\n"), None, "absurd size");
    }
}
