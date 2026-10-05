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

/// Peak resident set size of this process, in bytes (Linux `ru_maxrss`).
pub fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the struct we own and returns 0 on success.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc == 0 { (usage.ru_maxrss as u64) * 1024 } else { 0 }
}

/// Current resident memory split into anonymous (heap, the part that can
/// be out-of-memory killed) and file-backed (mapped files, reclaimable
/// cache), from `/proc/self/status`. `None` off Linux.
pub fn rss_breakdown() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_rss(&status)
}

fn parse_rss(status: &str) -> Option<(u64, u64)> {
    let mut anon = None;
    let mut file = None;
    for line in status.lines() {
        let kb = |l: &str| l.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()).map(|v| v * 1024);
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            anon = kb(rest.trim_start());
        } else if let Some(rest) = line.strip_prefix("RssFile:") {
            file = kb(rest.trim_start());
        }
    }
    Some((anon?, file?))
}

/// Bytes as megabytes with one decimal: `2_500_000` -> `"2.5 MB"`.
pub fn fmt_mb(bytes: u64) -> String {
    format!("{} MB", fmt_num(bytes as f64 / 1_000_000.0, 1))
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
    fn peak_rss_is_plausible() {
        let rss = peak_rss_bytes();
        assert!(rss > 1 << 20 && rss < 1 << 40, "{rss}");
        let (anon, file) = rss_breakdown().unwrap();
        assert!(anon > 0 && file > 0 && anon + file <= rss + (64 << 20), "{anon} {file} {rss}");
        assert_eq!(parse_rss("RssAnon:\t  1024 kB\nRssFile:\t     2 kB\n"), Some((1 << 20, 2048)));
        assert_eq!(parse_rss("RssAnon:\t 1 kB\n"), None);
        assert_eq!(parse_rss("garbage"), None);
    }

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
