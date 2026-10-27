//! Errors and the small helpers everything else uses: hex, varints, JSON
//! encoded exactly as Go's encoding/json does (object ids depend on it),
//! glob matching for .sprigignore, and local time formatting.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum Error {
    Msg(String),
    /// An object that is not in the store (and could not be fetched).
    NotFound(String),
    Io(std::io::Error),
    /// A remote server answered with an error status.
    Remote { code: u16, msg: String },
    /// Exit with this code, printing the message if it is not empty.
    Exit(i32, String),
    NothingToCommit,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[macro_export]
macro_rules! err {
    ($($t:tt)*) => { $crate::util::Error::Msg(format!($($t)*)) };
}

#[macro_export]
macro_rules! bail {
    ($($t:tt)*) => { return Err($crate::err!($($t)*)) };
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Msg(m) | Error::NotFound(m) | Error::Exit(_, m) => f.write_str(m),
            Error::Io(e) => write!(f, "{e}"),
            Error::Remote { code, msg } => write!(f, "remote: {msg} (HTTP {code})"),
            Error::NothingToCommit => f.write_str("nothing to commit"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Msg(e.to_string())
    }
}

impl Error {
    pub fn is_not_found(&self) -> bool {
        match self {
            Error::NotFound(_) => true,
            Error::Io(e) => e.kind() == std::io::ErrorKind::NotFound,
            _ => false,
        }
    }
}

/// Adds the path to an I/O error, as Go's *PathError does.
pub trait IoContext<T> {
    fn ctx(self, what: &str, path: &std::path::Path) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn ctx(self, what: &str, path: &std::path::Path) -> Result<T> {
        self.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Io(e) // callers test for NotFound
            } else {
                err!("{what} {}: {e}", path.display())
            }
        })
    }
}

// ---------------------------------------------------------------------------
// hex and varints

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn hex(b: &[u8]) -> String {
    let mut v = Vec::with_capacity(b.len() * 2);
    for &c in b {
        v.push(HEX[(c >> 4) as usize]);
        v.push(HEX[(c & 15) as usize]);
    }
    String::from_utf8(v).expect("hex is ASCII")
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let v = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    s.as_bytes().chunks(2).map(|p| Some(v(p[0])? << 4 | v(p[1])?)).collect()
}

/// The raw 32 bytes of a full object id.
pub fn raw_id(id: &str) -> Option<[u8; 32]> {
    unhex(id)?.try_into().ok()
}

pub fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads a uvarint, returning it and the bytes it took (None if malformed).
pub fn uvarint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &c) in b.iter().enumerate().take(10) {
        if i == 9 && c > 1 {
            return None; // overflows 64 bits
        }
        v |= ((c & 0x7f) as u64) << (7 * i);
        if c < 0x80 {
            return Some((v, i + 1));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// JSON as Go writes it: same field order and escaping, so that objects hash
// to the same ids as ones the Go version wrote.

/// Escapes what Go's encoder escapes and serde_json doesn't. These
/// characters only ever occur inside strings, so a pass over the output is
/// exact.
fn go_escape(b: Vec<u8>) -> Vec<u8> {
    let special = |i: usize, b: &[u8]| match b[i] {
        b'<' | b'>' | b'&' => true,
        0xe2 => b.len() > i + 2 && b[i + 1] == 0x80 && (b[i + 2] == 0xa8 || b[i + 2] == 0xa9),
        _ => false,
    };
    if !(0..b.len()).any(|i| special(i, &b)) {
        return b;
    }
    let mut out = Vec::with_capacity(b.len() + 16);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => out.extend_from_slice(b"\\u003c"),
            b'>' => out.extend_from_slice(b"\\u003e"),
            b'&' => out.extend_from_slice(b"\\u0026"),
            0xe2 if special(i, &b) => {
                out.extend_from_slice(if b[i + 2] == 0xa8 { b"\\u2028" } else { b"\\u2029" });
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// Appends bytes as a JSON string exactly as Go's encoder writes a Go
/// string holding them: bytes that are not valid UTF-8 become the escape
/// \ufffd. Used for text imported from git, which may be in any encoding.
pub fn go_json_str(out: &mut Vec<u8>, mut b: &[u8]) {
    out.push(b'"');
    while !b.is_empty() {
        let valid = match std::str::from_utf8(b) {
            Ok(s) => s,
            Err(e) => std::str::from_utf8(&b[..e.valid_up_to()]).unwrap(),
        };
        out.extend_from_slice(&go_escape(serde_json::to_vec(valid).unwrap())[1..]);
        out.pop(); // the closing quote
        b = &b[valid.len()..];
        if !b.is_empty() {
            out.extend_from_slice(b"\\ufffd");
            b = &b[1..];
        }
    }
    out.push(b'"');
}

pub fn to_json<T: Serialize + ?Sized>(v: &T) -> Vec<u8> {
    go_escape(serde_json::to_vec(v).expect("JSON encoding"))
}

pub fn to_json_pretty<T: Serialize + ?Sized>(v: &T) -> String {
    let b = go_escape(serde_json::to_vec_pretty(v).expect("JSON encoding"));
    String::from_utf8(b).expect("JSON is UTF-8")
}

/// Deserializes null as the default value, as Go does for slices and maps.
pub fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

pub fn is_false(b: &bool) -> bool {
    !*b
}

// ---------------------------------------------------------------------------
// time

pub fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

pub fn now_unix() -> i64 {
    now_ns() / 1_000_000_000
}

/// Broken-down local time: (year, month 1-12, day, hour, min, sec, weekday 0=Sunday, UTC offset in seconds).
#[cfg(unix)]
fn local(t: i64) -> (i64, usize, i64, i64, i64, i64, usize, i64) {
    // SAFETY: localtime_r only writes the tm we pass it.
    unsafe {
        let tt = t as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&tt, &mut tm);
        (
            tm.tm_year as i64 + 1900,
            tm.tm_mon as usize + 1,
            tm.tm_mday as i64,
            tm.tm_hour as i64,
            tm.tm_min as i64,
            tm.tm_sec as i64,
            tm.tm_wday as usize,
            tm.tm_gmtoff as i64,
        )
    }
}

#[cfg(not(unix))]
fn local(t: i64) -> (i64, usize, i64, i64, i64, i64, usize, i64) {
    // shortcut: no time zone database here, so times show in UTC
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let (y, m, d) = civil(days);
    (y, m, d, secs / 3600, secs / 60 % 60, secs % 60, (days + 4).rem_euclid(7) as usize, 0)
}

#[cfg(not(unix))]
fn civil(z: i64) -> (i64, usize, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m as usize, d)
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "Mon Jan 2 15:04:05 2006" in local time.
pub fn fmt_date(t: i64) -> String {
    let (y, mo, d, h, mi, s, wd, _) = local(t);
    format!("{} {} {d} {h:02}:{mi:02}:{s:02} {y}", DAYS[wd], MONTHS[mo - 1])
}

/// "2006-01-02 15:04:05" in local time.
pub fn fmt_datetime(t: i64) -> String {
    let (y, mo, d, h, mi, s, _, _) = local(t);
    format!("{y}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// The local UTC offset now, as "-0700".
pub fn tz_now() -> String {
    let off = local(now_unix()).7;
    let sign = if off < 0 { '-' } else { '+' };
    let m = off.abs() / 60;
    format!("{sign}{:02}{:02}", m / 60, m % 60)
}

/// Bytes as a string the way Go's JSON encoder sees them: each byte that is
/// not part of valid UTF-8 becomes U+FFFD. Object ids of imported history
/// depend on it.
pub fn go_lossy(mut b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    loop {
        match std::str::from_utf8(b) {
            Ok(v) => return s + v,
            Err(e) => {
                let (ok, rest) = b.split_at(e.valid_up_to());
                s.push_str(std::str::from_utf8(ok).unwrap());
                s.push('\u{FFFD}');
                b = &rest[1..];
            }
        }
    }
}

// ---------------------------------------------------------------------------
// paths

/// Go's path.Match: '*' and '?' never match '/', '[...]' classes, '\\'
/// escapes. A malformed pattern matches nothing.
pub fn path_match(pat: &str, name: &str) -> bool {
    glob(pat.as_bytes(), name.as_bytes()).unwrap_or(false)
}

fn glob(p: &[u8], s: &[u8]) -> Option<bool> {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None; // pattern after '*', where in s it resumes
    loop {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star = Some((pi + 1, si));
                    pi += 1;
                    continue;
                }
                c => {
                    if si < s.len() {
                        let (ok, next) = match c {
                            b'?' => (s[si] != b'/', pi + 1),
                            b'[' => class(p, pi + 1, s[si])?,
                            b'\\' => (p.get(pi + 1)? == &s[si], pi + 2),
                            c => (c == s[si], pi + 1),
                        };
                        if ok {
                            pi = next;
                            si += 1;
                            continue;
                        }
                    } else if c == b'[' || c == b'\\' {
                        // still validate the rest of the pattern, as Go does
                        if c == b'[' {
                            class(p, pi + 1, 0)?;
                        } else {
                            p.get(pi + 1)?;
                        }
                    }
                }
            }
        } else if si == s.len() {
            return Some(true);
        }
        // mismatch: let the last '*' swallow one more byte, but never a '/'
        match star {
            Some((sp, ss)) if ss < s.len() && s[ss] != b'/' => {
                pi = sp;
                si = ss + 1;
                star = Some((sp, ss + 1));
            }
            _ => {
                // check the rest of the pattern is well formed
                let mut i = pi;
                while i < p.len() {
                    match p[i] {
                        b'[' => i = class(p, i + 1, 0)?.1,
                        b'\\' => {
                            p.get(i + 1)?;
                            i += 2
                        }
                        _ => i += 1,
                    }
                }
                return Some(false);
            }
        }
    }
}

/// Matches c against the class starting after '['; returns (matched, index after ']').
fn class(p: &[u8], mut i: usize, c: u8) -> Option<(bool, usize)> {
    let negate = p.get(i) == Some(&b'^');
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let lo = *p.get(i)?;
        if lo == b']' && !first {
            return Some((matched != negate && c != b'/', i + 1));
        }
        first = false;
        let lo = if lo == b'\\' {
            i += 1;
            *p.get(i)?
        } else {
            lo
        };
        i += 1;
        let mut hi = lo;
        if p.get(i) == Some(&b'-') && p.get(i + 1).is_some_and(|&n| n != b']') {
            i += 1;
            hi = *p.get(i)?;
            if hi == b'\\' {
                i += 1;
                hi = *p.get(i)?;
            }
            i += 1;
        }
        if lo <= c && c <= hi {
            matched = true;
        }
    }
}

pub fn base_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Go's path.Dir for slash paths ("." when there is no directory).
pub fn dir_name(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => ".",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        for (p, s, want) in [
            ("*.log", "x.log", true),
            ("*.log", "d/x.log", false),
            ("build", "build", true),
            ("a?c", "abc", true),
            ("a?c", "a/c", false),
            ("[a-c]x", "bx", true),
            ("[^a-c]x", "bx", false),
            ("*", "", true),
            ("a*b*c", "axxbyyc", true),
            ("a*b", "a/b", false),
            ("[", "a", false),
            ("\\*", "*", true),
        ] {
            assert_eq!(path_match(p, s), want, "{p} {s}");
        }
    }

    #[test]
    fn go_json_escaping() {
        let s = String::from_utf8(to_json(&"a<b>&\u{2028}\n")).unwrap();
        assert_eq!(s, ["\"a", "\\u003cb", "\\u003e", "\\u0026", "\\u2028", "\\n\""].concat());
    }
}
