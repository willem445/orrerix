//! Fallible, bounded whole-file reads for the periodic poll paths (#3469).
//!
//! **Why this exists.** orrerix 1.3.1-beta3 died with `allocation of 1048576
//! bytes (align 8) was refused by the system allocator` while an unrelated
//! process held the machine at its commit limit. An infallible grow that the
//! allocator refuses goes `handle_alloc_error` → abort, and no caller gets a
//! say: the whole app dies to redraw a chart. The readers the UI and the
//! derivations poll are the sites that ask for the most memory at once, so
//! they are the ones that should be able to answer "not this tick" instead.
//!
//! **What "fallible" covers here, precisely.** The one large, contiguous
//! buffer a whole-file read needs: its capacity is reserved with
//! [`String::try_reserve_exact`] and every growth past it goes through
//! [`Vec::try_reserve`], so a refusal comes back as
//! [`BoundedReadError::Refused`] instead of an abort. `std::fs::read_to_string`
//! already reserves its size hint fallibly, but std documents that as "not
//! guaranteed", and a read that grows past the hint (a file appended to
//! between `metadata` and `read`) grows through std's own probe loop, whose
//! contract we do not own. This one we do.
//!
//! **What it does not cover, and cannot.** The small allocations a caller
//! makes *parsing* what was read — a `serde_json::Value` per line, a `String`
//! per field — stay infallible: no parser in the tree has a fallible mode, and
//! a machine refusing a few hundred bytes is past the point where any one read
//! degrading could keep the app alive. Those stay covered by the crash record
//! (`obs::CrashReportingAlloc`), which is the division of labour
//! `docs/design/crash-observability.md` §1c states.
//!
//! **The limit is the fixture seam as well as a ceiling.** A read of a file
//! larger than `limit` is refused before any buffer is reserved
//! ([`BoundedReadError::TooLarge`]). In production that is a sanity cap sized
//! well above anything the file should legitimately reach; in a test it is how
//! a refusal is injected without exhausting memory — the refusal path is the
//! same one a real `Refused` takes from the caller's point of view (an `Err`
//! it has to report), so pinning one pins the caller's handling of both.

use std::collections::{TryReserveError, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Chunk size for the read loop. The buffer is reserved up front from the
/// file's metadata, so in the ordinary case no chunk ever grows it; this only
/// bounds how much a file that is still being appended to can add per
/// fallible reservation.
const CHUNK: usize = 64 * 1024;

/// Why a bounded read did not produce the file's text.
#[derive(Debug)]
pub enum BoundedReadError {
    /// Opening, sizing or reading the file failed. `NotFound` is the one kind
    /// most callers treat as "empty"; [`BoundedReadError::is_not_found`] says
    /// so without the caller matching on `io::ErrorKind`.
    Io(io::Error),
    /// The file is larger than the caller's limit — either on disk before the
    /// read, or by growing past it during the read. Nothing past `limit` was
    /// buffered.
    TooLarge { len: u64, limit: u64 },
    /// The allocator refused the buffer. The request that was refused is
    /// `bytes` of **additional** capacity (the same figure a crash record
    /// would have carried, had this been an infallible grow).
    Refused { bytes: usize },
    /// The bytes are not UTF-8. Same outcome `fs::read_to_string` gives.
    NotUtf8,
}

impl BoundedReadError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, BoundedReadError::Io(e) if e.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for BoundedReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoundedReadError::Io(e) => write!(f, "read failed: {e}"),
            BoundedReadError::TooLarge { len, limit } => {
                write!(f, "file is {len} bytes, over the {limit}-byte read limit")
            }
            BoundedReadError::Refused { bytes } => {
                write!(f, "the allocator refused {bytes} bytes for the read buffer")
            }
            BoundedReadError::NotUtf8 => write!(f, "file is not valid UTF-8"),
        }
    }
}

impl std::error::Error for BoundedReadError {}

impl From<io::Error> for BoundedReadError {
    fn from(e: io::Error) -> Self {
        BoundedReadError::Io(e)
    }
}

/// Map a refused reservation onto the error the caller reports. The
/// `TryReserveError` itself carries no public size, so the request is passed
/// in rather than recovered from it.
fn refused(bytes: usize) -> impl FnOnce(TryReserveError) -> BoundedReadError {
    move |_| BoundedReadError::Refused { bytes }
}

/// Read `path` whole as UTF-8, refusing a file over `limit` bytes and turning
/// every buffer reservation into a fallible one.
///
/// Equivalent to `fs::read_to_string` for a file under the limit on a machine
/// with memory to spare — same bytes, same `NotFound`, same refusal of
/// non-UTF-8 — and different only in the two cases this exists for: the
/// limit, and a refused allocation, both of which return `Err` rather than
/// buffering past the limit or aborting the process.
pub fn read_to_string_bounded(path: &Path, limit: u64) -> Result<String, BoundedReadError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > limit {
        return Err(BoundedReadError::TooLarge { len, limit });
    }
    // `len <= limit`, and a limit that does not fit `usize` has no business
    // being a limit on a buffer, so the cast saturates rather than wraps.
    let hint = usize::try_from(len).unwrap_or(usize::MAX);
    let mut buf: Vec<u8> = Vec::new();
    buf.try_reserve_exact(hint).map_err(refused(hint))?;
    // `take(limit + 1)`: one byte past the limit is enough to know the file
    // grew over it, and nothing further is ever pulled into memory.
    let mut src = (&mut file).take(limit.saturating_add(1));
    loop {
        // Ordinarily the up-front reservation already covers the file and this
        // never fires; it is the path a file still being appended to takes.
        if buf.capacity() == buf.len() {
            buf.try_reserve(CHUNK).map_err(refused(CHUNK))?;
        }
        let start = buf.len();
        let want = (buf.capacity() - start).min(CHUNK);
        buf.resize(start + want, 0); // within capacity: cannot allocate
        let n = match src.read(&mut buf[start..]) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                buf.truncate(start);
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        buf.truncate(start + n);
        if n == 0 {
            break;
        }
        if buf.len() as u64 > limit {
            return Err(BoundedReadError::TooLarge { len: buf.len() as u64, limit });
        }
    }
    // `from_utf8` validates in place: no second buffer.
    String::from_utf8(buf).map_err(|_| BoundedReadError::NotUtf8)
}

/// `Vec::push`, with the growth made fallible. Parsers that collect an
/// unbounded number of rows use this instead of `collect()`, whose growth is
/// infallible — the typed row `Vec` is the align-8 allocation class the
/// #3469 record names, not the byte buffer.
pub fn try_push<T>(v: &mut Vec<T>, item: T) -> Result<(), TryReserveError> {
    v.try_reserve(1)?;
    v.push(item);
    Ok(())
}

/// Make room for one more element in a deque that is never meant to hold
/// more than `cap` (#3493 review N2). Growth doubles, as `push_back` would,
/// but fallibly and **capped**: capacity never exceeds `cap`, so a window that
/// pops before it pushes at `cap` stops growing there, and a short log never
/// reserves the whole window. A no-op while there is spare capacity, and at
/// `cap` itself (the caller pops first).
pub fn try_grow_capped<T>(d: &mut VecDeque<T>, cap: usize) -> Result<(), TryReserveError> {
    let len = d.len();
    if len < d.capacity() || len >= cap {
        return Ok(());
    }
    d.try_reserve_exact(len.max(16).min(cap - len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file_with(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.jsonl");
        fs::write(&p, bytes).unwrap();
        (d, p)
    }

    #[test]
    fn a_file_over_the_limit_is_an_err_and_one_at_it_is_read_whole() {
        let (_d, p) = file_with(b"0123456789");
        assert_eq!(read_to_string_bounded(&p, 10).unwrap(), "0123456789");
        match read_to_string_bounded(&p, 9) {
            Err(BoundedReadError::TooLarge { len: 10, limit: 9 }) => {}
            other => panic!("one byte over the limit must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_multi_chunk_file_reads_byte_identical_to_fs_read_to_string() {
        // Several `CHUNK`s plus a remainder, so the loop runs more than once.
        let text: String = (0..(3 * CHUNK + 17)).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let (_d, p) = file_with(text.as_bytes());
        let got = read_to_string_bounded(&p, u64::MAX).unwrap();
        assert!(got == fs::read_to_string(&p).unwrap(), "same bytes as std's read");
    }

    #[test]
    fn missing_empty_and_non_utf8_keep_read_to_strings_outcomes() {
        let d = tempfile::tempdir().unwrap();
        let missing = read_to_string_bounded(&d.path().join("absent"), 10).unwrap_err();
        assert!(missing.is_not_found(), "a missing file is NotFound: {missing}");
        let (_e, empty) = file_with(b"");
        assert_eq!(read_to_string_bounded(&empty, 0).unwrap(), "");
        let (_n, bad) = file_with(&[0x66, 0xff, 0x0a]);
        assert!(matches!(read_to_string_bounded(&bad, 10), Err(BoundedReadError::NotUtf8)));
    }

    #[test]
    fn a_capped_deque_grows_on_demand_and_never_past_its_cap() {
        let cap = 100;
        let mut d: VecDeque<u32> = VecDeque::new();
        assert_eq!(d.capacity(), 0, "nothing is reserved before the first element");
        try_grow_capped(&mut d, cap).unwrap();
        d.push_back(0);
        assert!(d.capacity() < cap, "one element does not reserve the whole window: {}", d.capacity());
        for i in 1..1000u32 {
            if d.len() == cap {
                d.pop_front();
            }
            try_grow_capped(&mut d, cap).unwrap();
            d.push_back(i);
            assert!(d.capacity() <= cap, "capacity {} passed the cap at {i}", d.capacity());
        }
        assert_eq!(d.len(), cap);
        assert_eq!(d.front(), Some(&900), "the oldest are the ones gone");
    }

    #[test]
    fn try_push_grows_like_push() {
        let mut v = Vec::new();
        for i in 0..100u64 {
            try_push(&mut v, i).unwrap();
        }
        assert_eq!(v, (0..100u64).collect::<Vec<_>>());
    }
}
