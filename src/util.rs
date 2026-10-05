//! Small formatting and hex helpers shared by the commands.

use std::path::Path;

/// `1234567` -> `"1,234,567"`.
pub fn fmt_int(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Fixed decimals with a thousands separator in the integer part.
pub fn fmt_num(x: f64, decimals: usize) -> String {
    if !x.is_finite() {
        return "n/a".to_string();
    }
    let s = format!("{x:.decimals$}");
    let (int_part, frac) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (s.clone(), None),
    };
    let (sign, digits) = match int_part.strip_prefix('-') {
        Some(d) => ("-", d.to_string()),
        None => ("", int_part),
    };
    let grouped = fmt_int(digits.parse::<u64>().unwrap_or(0));
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

/// Bytes per second as a human-readable rate.
pub fn fmt_rate(bytes: u64, secs: f64) -> String {
    if secs <= 0.0 {
        return "n/a".to_string();
    }
    let mb = bytes as f64 / 1_000_000.0 / secs;
    format!("{mb:.1} MB/s")
}

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Total size of every regular file under `dir`, following nothing.
pub fn dir_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints_get_commas() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1000), "1,000");
        assert_eq!(fmt_int(1234567), "1,234,567");
    }

    #[test]
    fn nums_keep_decimals() {
        assert_eq!(fmt_num(1234.5, 2), "1,234.50");
        assert_eq!(fmt_num(-1234.5, 1), "-1,234.5");
        assert_eq!(fmt_num(7.0, 0), "7");
        assert_eq!(fmt_num(f64::NAN, 2), "n/a");
        assert_eq!(fmt_num(0.004, 2), "0.00");
    }

    #[test]
    fn rates() {
        assert_eq!(fmt_rate(2_000_000, 2.0), "1.0 MB/s");
        assert_eq!(fmt_rate(1, 0.0), "n/a");
    }

    #[test]
    fn hex_round_trip() {
        let bytes = [0u8, 1, 0xab, 0xff];
        let h = to_hex(&bytes);
        assert_eq!(h, "0001abff");
        assert_eq!(from_hex(&h).unwrap(), bytes);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
        assert_eq!(from_hex("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn dir_sizes_recurse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"12345").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), b"123").unwrap();
        std::os::unix::fs::symlink(dir.path().join("a"), dir.path().join("link")).unwrap();
        assert_eq!(dir_size(dir.path()).unwrap(), 8);
        assert!(dir_size(&dir.path().join("missing")).is_err());
    }
}
