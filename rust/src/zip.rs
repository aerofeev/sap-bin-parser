//! Just enough zip to read SAP exports as a stream and to write results.
//!
//! Three pieces:
//!
//! * [`StreamReader`] walks local file headers front to back over any
//!   [`Read`], so an upload can be converted while it is still arriving.
//!   STORED and DEFLATE entries, data descriptors (including STORED entries
//!   with descriptors, found by signature and confirmed by CRC and length),
//!   and zip64 are handled. Nothing is buffered beyond a read window.
//! * [`IndexedReader`] uses the central directory of a seekable file, so a
//!   local file is processed in shard order wherever its sidecar sits.
//! * [`ZipWriter`] writes archives to any [`Write`], including a
//!   non-seekable HTTP response, using data descriptors and zip64 as needed.
//!
//! Every entry's CRC-32 is verified, so a truncated or corrupted upload is an
//! error rather than silently short output.

use std::io::{self, Read, Seek, SeekFrom, Write};

use flate2::{Decompress, FlushDecompress, Status};

const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL: u32 = 0x0605_4b50;
const ZIP64_END: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;
const DESCRIPTOR_BYTES: [u8; 4] = [0x50, 0x4b, 0x07, 0x08];

const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_DESCRIPTOR: u16 = 0x0008;
const FLAG_UTF8: u16 = 0x0800;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn truncated() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the archive ends unexpectedly: it is truncated, or the upload was interrupted",
    )
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

/// A buffered reader with lookahead, over any byte stream.
pub(crate) struct ByteReader<R> {
    inner: R,
    buf: Vec<u8>,
    start: usize,
    end: usize,
    eof: bool,
}

const WINDOW: usize = 256 * 1024;

impl<R: Read> ByteReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            buf: vec![0; WINDOW],
            start: 0,
            end: 0,
            eof: false,
        }
    }

    fn reset(&mut self) {
        self.start = 0;
        self.end = 0;
        self.eof = false;
    }

    /// Whatever is buffered, reading more if nothing is. Empty at EOF.
    fn fill(&mut self) -> io::Result<&[u8]> {
        if self.start == self.end && !self.eof {
            self.start = 0;
            self.end = 0;
            let n = read_retrying(&mut self.inner, &mut self.buf)?;
            self.end = n;
            self.eof = n == 0;
        }
        Ok(&self.buf[self.start..self.end])
    }

    /// At least `n` buffered bytes, unless the stream ends first.
    fn fill_at_least(&mut self, n: usize) -> io::Result<&[u8]> {
        while self.end - self.start < n && !self.eof {
            if self.start > 0 {
                self.buf.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            if self.buf.len() < n {
                self.buf.resize(n.max(WINDOW), 0);
            }
            let end = self.end;
            let got = read_retrying(&mut self.inner, &mut self.buf[end..])?;
            if got == 0 {
                self.eof = true;
            }
            self.end += got;
        }
        Ok(&self.buf[self.start..self.end])
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
        debug_assert!(self.start <= self.end);
    }

    fn read_exact(&mut self, out: &mut [u8]) -> io::Result<()> {
        let got = self.fill_at_least(out.len())?;
        if got.len() < out.len() {
            return Err(truncated());
        }
        out.copy_from_slice(&got[..out.len()]);
        self.consume(out.len());
        Ok(())
    }

    fn take(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0; n];
        self.read_exact(&mut out)?;
        Ok(out)
    }

    fn skip(&mut self, mut n: u64) -> io::Result<()> {
        while n > 0 {
            let got = self.fill()?;
            if got.is_empty() {
                return Err(truncated());
            }
            let step = (got.len() as u64).min(n) as usize;
            self.consume(step);
            n -= step as u64;
        }
        Ok(())
    }

    /// Read and discard everything left in the stream.
    pub(crate) fn drain(&mut self) -> io::Result<()> {
        loop {
            let n = self.fill()?.len();
            if n == 0 {
                return Ok(());
            }
            self.consume(n);
        }
    }

    fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }
}

fn read_retrying(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// What is known about an entry from its header.
#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub name: String,
    pub method: u16,
    /// Uncompressed size, when the header states it.
    pub size: Option<u64>,
    /// Compressed size, when the header states it.
    pub compressed_size: Option<u64>,
}

impl EntryInfo {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

enum Body {
    Stored {
        remaining: u64,
    },
    /// A STORED entry whose length is only given by a trailing descriptor.
    StoredScan,
    Deflate {
        inflater: Box<Decompress>,
        remaining_in: Option<u64>,
    },
}

struct Active {
    info: EntryInfo,
    body: Body,
    crc: crc32fast::Hasher,
    produced: u64,
    expect_crc: Option<u32>,
    descriptor: bool,
    zip64: bool,
    done: bool,
}

impl Active {
    fn new(
        info: EntryInfo,
        flags: u16,
        crc: u32,
        zip64: bool,
        sizes_known: bool,
    ) -> io::Result<Self> {
        if flags & FLAG_ENCRYPTED != 0 {
            return Err(invalid(format!(
                "{} is encrypted; remove the password and try again",
                info.name
            )));
        }
        let descriptor = flags & FLAG_DESCRIPTOR != 0 && !sizes_known;
        let compressed = if descriptor && info.compressed_size == Some(0) {
            None
        } else {
            info.compressed_size
        };
        let body = match info.method {
            METHOD_STORED => match compressed {
                Some(remaining) => Body::Stored { remaining },
                None => Body::StoredScan,
            },
            METHOD_DEFLATE => Body::Deflate {
                inflater: Box::new(Decompress::new(false)),
                remaining_in: compressed,
            },
            other => {
                return Err(invalid(format!(
                    "{} uses zip compression method {other}, which is not supported; \
                     re-zip it with standard Deflate",
                    info.name
                )))
            }
        };
        Ok(Self {
            info,
            body,
            crc: crc32fast::Hasher::new(),
            produced: 0,
            expect_crc: if descriptor { None } else { Some(crc) },
            descriptor,
            zip64,
            done: false,
        })
    }

    fn read<R: Read>(&mut self, input: &mut ByteReader<R>, out: &mut [u8]) -> io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        let n = match &mut self.body {
            Body::Stored { remaining } => {
                if *remaining == 0 {
                    0
                } else {
                    let got = input.fill()?;
                    if got.is_empty() {
                        return Err(truncated());
                    }
                    let n = got.len().min(out.len()).min(*remaining as usize);
                    out[..n].copy_from_slice(&got[..n]);
                    input.consume(n);
                    *remaining -= n as u64;
                    n
                }
            }
            Body::Deflate {
                inflater,
                remaining_in,
            } => loop {
                let got = input.fill()?;
                let limit = remaining_in.map_or(got.len(), |r| got.len().min(r as usize));
                let slice = &got[..limit];
                let (before_in, before_out) = (inflater.total_in(), inflater.total_out());
                let status = inflater
                    .decompress(slice, out, FlushDecompress::None)
                    .map_err(|e| {
                        invalid(format!("{}: corrupt deflate data ({e})", self.info.name))
                    })?;
                let consumed = (inflater.total_in() - before_in) as usize;
                let produced = (inflater.total_out() - before_out) as usize;
                let exhausted = slice.is_empty();
                input.consume(consumed);
                if let Some(r) = remaining_in {
                    *r -= consumed as u64;
                }
                if status == Status::StreamEnd {
                    if let Some(r) = remaining_in.take() {
                        input.skip(r)?;
                    }
                    self.body = Body::Stored { remaining: 0 };
                    break produced;
                }
                if produced > 0 {
                    break produced;
                }
                if consumed == 0 && exhausted {
                    return Err(truncated());
                }
            },
            Body::StoredScan => return self.read_scanning(input, out),
        };
        if n == 0 {
            self.finish(input)?;
            return Ok(0);
        }
        self.crc.update(&out[..n]);
        self.produced += n as u64;
        Ok(n)
    }

    /// A STORED entry of unknown length ends at a data descriptor whose CRC
    /// and size match everything before it. A descriptor signature that does
    /// not check out is ordinary data.
    fn read_scanning<R: Read>(
        &mut self,
        input: &mut ByteReader<R>,
        out: &mut [u8],
    ) -> io::Result<usize> {
        enum Step {
            Emit(usize),
            End { data: usize, crc: u32 },
        }
        // Signature, CRC, then two sizes of 4 or 8 bytes.
        let need = if self.zip64 { 24 } else { 16 };
        let step = {
            let got = input.fill_at_least(need + 64 * 1024)?;
            let mut from = 0;
            let mut step = None;
            while let Some(found) = find(&got[from..], &DESCRIPTOR_BYTES) {
                let p = from + found;
                if got.len() < p + need {
                    // Too little lookahead to judge this candidate; everything
                    // before it is certainly data.
                    step = Some(if p > 0 {
                        Step::Emit(p)
                    } else {
                        return Err(truncated());
                    });
                    break;
                }
                let crc = le32(&got[p + 4..]);
                let size = if self.zip64 {
                    le64(&got[p + 8..])
                } else {
                    le32(&got[p + 8..]) as u64
                };
                if size == self.produced + p as u64 {
                    let mut hasher = self.crc.clone();
                    hasher.update(&got[..p]);
                    if hasher.finalize() == crc {
                        step = Some(Step::End { data: p, crc });
                        break;
                    }
                }
                from = p + 1;
            }
            match step {
                Some(step) => step,
                // No descriptor in view: emit all but a possible partial
                // signature at the very end.
                None if got.len() > 3 => Step::Emit(got.len() - 3),
                None => return Err(truncated()),
            }
        };
        match step {
            Step::Emit(n) => Ok(self.emit(input, out, n.min(out.len()))),
            Step::End { data, .. } if data > out.len() => Ok(self.emit(input, out, out.len())),
            Step::End { data, crc } => {
                let n = self.emit(input, out, data);
                input.consume(need);
                self.expect_crc = Some(crc);
                self.descriptor = false;
                self.done = true;
                self.verify()?;
                Ok(n)
            }
        }
    }

    fn emit<R: Read>(&mut self, input: &mut ByteReader<R>, out: &mut [u8], n: usize) -> usize {
        let got = &input.buf[input.start..input.start + n];
        out[..n].copy_from_slice(got);
        self.crc.update(got);
        self.produced += n as u64;
        input.consume(n);
        n
    }

    fn finish<R: Read>(&mut self, input: &mut ByteReader<R>) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        if self.descriptor {
            let head = input.fill_at_least(4)?;
            if head.len() >= 4 && le32(head) == DESCRIPTOR {
                input.consume(4);
            }
            let mut crc = [0u8; 4];
            input.read_exact(&mut crc)?;
            input.skip(if self.zip64 { 16 } else { 8 })?;
            self.expect_crc = Some(le32(&crc));
        }
        self.verify()
    }

    fn verify(&self) -> io::Result<()> {
        if let Some(expected) = self.expect_crc {
            if self.crc.clone().finalize() != expected {
                return Err(invalid(format!(
                    "{}: CRC mismatch; the archive is corrupt or was damaged in transit",
                    self.info.name
                )));
            }
        }
        if let Some(size) = self.info.size {
            if !self.descriptor && size != self.produced && self.expect_crc.is_some() {
                return Err(invalid(format!(
                    "{}: expected {size} bytes, got {}",
                    self.info.name, self.produced
                )));
            }
        }
        Ok(())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn decode_name(raw: &[u8], flags: u16) -> String {
    if flags & FLAG_UTF8 != 0 {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        // CP437 for anything non-ASCII; only ASCII matters for shard names.
        raw.iter()
            .map(|&b| if b.is_ascii() { b as char } else { '\u{FFFD}' })
            .collect()
    }
}

/// Parse the zip64 extended-information extra field: 8-byte values present
/// only for header fields that were 0xFFFFFFFF, in the fixed order
/// (size, compressed size, local header offset).
fn zip64_extra(extra: &[u8], want: [bool; 3]) -> (bool, [Option<u64>; 3]) {
    let mut i = 0;
    while i + 4 <= extra.len() {
        let id = le16(&extra[i..]);
        let len = le16(&extra[i + 2..]) as usize;
        let body = &extra[(i + 4).min(extra.len())..(i + 4 + len).min(extra.len())];
        if id == 0x0001 {
            let mut values = [None; 3];
            let mut at = 0;
            for (slot, wanted) in want.iter().enumerate() {
                if *wanted && at + 8 <= body.len() {
                    values[slot] = Some(le64(&body[at..]));
                    at += 8;
                }
            }
            return (true, values);
        }
        i += 4 + len;
    }
    (false, [None; 3])
}

/// A source of zip entries whose [`Read`] yields the current entry's bytes.
pub trait Entries: Read {
    /// Advance to the next entry, discarding whatever is left of the current
    /// one. `None` once there are no more.
    fn next_entry(&mut self) -> io::Result<Option<EntryInfo>>;
}

/// Reads a zip archive front to back from a non-seekable stream.
pub struct StreamReader<R> {
    input: ByteReader<R>,
    active: Option<Active>,
    finished: bool,
}

impl<R: Read> StreamReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            input: ByteReader::new(inner),
            active: None,
            finished: false,
        }
    }

    /// Consume the rest of the underlying stream (for example the central
    /// directory at the end of an upload).
    pub fn drain(&mut self) -> io::Result<()> {
        self.input.drain()
    }

    pub fn into_inner(self) -> R {
        self.input.inner
    }
}

impl<R: Read> Read for StreamReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match &mut self.active {
            Some(active) => active.read(&mut self.input, out),
            None => Ok(0),
        }
    }
}

impl<R: Read> Entries for StreamReader<R> {
    fn next_entry(&mut self) -> io::Result<Option<EntryInfo>> {
        if let Some(mut active) = self.active.take() {
            let mut scratch = vec![0u8; 64 * 1024];
            while active.read(&mut self.input, &mut scratch)? > 0 {}
        }
        if self.finished {
            return Ok(None);
        }
        let head = self.input.fill_at_least(4)?;
        if head.is_empty() {
            self.finished = true;
            return Ok(None);
        }
        if head.len() < 4 {
            return Err(truncated());
        }
        match le32(head) {
            LOCAL_HEADER => {}
            CENTRAL_HEADER | END_OF_CENTRAL | ZIP64_END => {
                self.finished = true;
                return Ok(None);
            }
            _ => return Err(invalid("this is not a zip archive, or it is damaged")),
        }
        let header = self.input.take(30)?;
        let flags = le16(&header[6..]);
        let method = le16(&header[8..]);
        let crc = le32(&header[14..]);
        let compressed = le32(&header[18..]);
        let size = le32(&header[22..]);
        let name_len = le16(&header[26..]) as usize;
        let extra_len = le16(&header[28..]) as usize;
        let name = decode_name(&self.input.take(name_len)?, flags);
        let extra = self.input.take(extra_len)?;
        let (zip64, values) =
            zip64_extra(&extra, [size == u32::MAX, compressed == u32::MAX, false]);
        let info = EntryInfo {
            name,
            method,
            size: Some(values[0].unwrap_or(size as u64)),
            compressed_size: Some(values[1].unwrap_or(compressed as u64)),
        };
        let info = if flags & FLAG_DESCRIPTOR != 0 {
            EntryInfo {
                size: None,
                compressed_size: info.compressed_size.filter(|&c| c != 0),
                ..info
            }
        } else {
            info
        };
        let mut active = Active::new(info.clone(), flags, crc, zip64, false)?;
        if active.info.compressed_size.is_none() && method == METHOD_STORED {
            active.body = Body::StoredScan;
        }
        self.active = Some(active);
        Ok(Some(info))
    }
}

/// One central-directory record.
#[derive(Debug, Clone)]
pub struct CentralEntry {
    pub name: String,
    pub method: u16,
    pub flags: u16,
    pub crc: u32,
    pub compressed_size: u64,
    pub size: u64,
    pub offset: u64,
}

/// Locate and parse the central directory, given a way to read byte ranges
/// and the total size. Used both on seekable files and on the tail of an
/// upload that the browser sends for a quick manifest.
pub fn read_central_directory(
    total: u64,
    search: usize,
    read_at: &mut dyn FnMut(u64, usize) -> io::Result<Vec<u8>>,
) -> io::Result<Vec<CentralEntry>> {
    // The end record is 22 bytes plus a comment of up to 64 KiB.
    let window = total.min(search.min(65_557 + 22) as u64) as usize;
    let tail = read_at(total - window as u64, window)?;
    let at = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| le32(&tail[i..]) == END_OF_CENTRAL)
        .ok_or_else(|| {
            invalid("no zip central directory found; the file is not a complete zip archive")
        })?;
    let eocd = &tail[at..];
    let mut count = le16(&eocd[10..]) as u64;
    let mut cd_size = le32(&eocd[12..]) as u64;
    let mut cd_offset = le32(&eocd[16..]) as u64;
    let eocd_pos = total - window as u64 + at as u64;

    if (count == 0xFFFF || cd_size == u32::MAX as u64 || cd_offset == u32::MAX as u64)
        && eocd_pos >= 20
    {
        let locator = read_at(eocd_pos - 20, 20)?;
        if le32(&locator) == ZIP64_LOCATOR {
            let record = read_at(le64(&locator[8..]), 56)?;
            if le32(&record) != ZIP64_END {
                return Err(invalid("damaged zip64 end of central directory"));
            }
            count = le64(&record[32..]);
            cd_size = le64(&record[40..]);
            cd_offset = le64(&record[48..]);
        }
    }

    let cd = read_at(cd_offset, cd_size as usize)?;
    let mut entries = Vec::with_capacity(count as usize);
    let mut i = 0;
    while i + 46 <= cd.len() && le32(&cd[i..]) == CENTRAL_HEADER {
        let h = &cd[i..];
        let flags = le16(&h[8..]);
        let method = le16(&h[10..]);
        let crc = le32(&h[16..]);
        let compressed = le32(&h[20..]);
        let size = le32(&h[24..]);
        let name_len = le16(&h[28..]) as usize;
        let extra_len = le16(&h[30..]) as usize;
        let comment_len = le16(&h[32..]) as usize;
        let offset = le32(&h[42..]);
        let end = 46 + name_len + extra_len;
        if i + end > cd.len() {
            return Err(invalid("damaged zip central directory"));
        }
        let name = decode_name(&h[46..46 + name_len], flags);
        let (_, values) = zip64_extra(
            &h[46 + name_len..end],
            [size == u32::MAX, compressed == u32::MAX, offset == u32::MAX],
        );
        entries.push(CentralEntry {
            name,
            method,
            flags,
            crc,
            size: values[0].unwrap_or(size as u64),
            compressed_size: values[1].unwrap_or(compressed as u64),
            offset: values[2].unwrap_or(offset as u64),
        });
        i += end + comment_len;
    }
    Ok(entries)
}

/// Reads entries of a seekable archive through its central directory, in an
/// order the caller chooses.
pub struct IndexedReader<R> {
    input: ByteReader<R>,
    entries: Vec<CentralEntry>,
    next: usize,
    active: Option<Active>,
}

impl<R: Read + Seek> IndexedReader<R> {
    pub fn new(mut inner: R) -> io::Result<Self> {
        let total = inner.seek(SeekFrom::End(0))?;
        let entries = read_central_directory(total, 65_557 + 22, &mut |offset, len| {
            inner.seek(SeekFrom::Start(offset))?;
            let mut buf = vec![0; len];
            inner.read_exact(&mut buf).map_err(|_| truncated())?;
            Ok(buf)
        })?;
        Ok(Self {
            input: ByteReader::new(inner),
            entries,
            next: 0,
            active: None,
        })
    }

    pub fn entries(&self) -> &[CentralEntry] {
        &self.entries
    }

    /// Reorder the entries still to be read.
    pub fn sort_by_key<K: Ord>(&mut self, key: impl FnMut(&CentralEntry) -> K) {
        self.entries[self.next..].sort_by_key(key);
    }
}

impl<R: Read + Seek> Read for IndexedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match &mut self.active {
            Some(active) => active.read(&mut self.input, out),
            None => Ok(0),
        }
    }
}

impl<R: Read + Seek> Entries for IndexedReader<R> {
    fn next_entry(&mut self) -> io::Result<Option<EntryInfo>> {
        self.active = None;
        let Some(entry) = self.entries.get(self.next).cloned() else {
            return Ok(None);
        };
        self.next += 1;
        self.input.get_mut().seek(SeekFrom::Start(entry.offset))?;
        self.input.reset();
        let header = self.input.take(30)?;
        if le32(&header) != LOCAL_HEADER {
            return Err(invalid(format!("{}: damaged local header", entry.name)));
        }
        let skip = le16(&header[26..]) as u64 + le16(&header[28..]) as u64;
        self.input.skip(skip)?;
        let info = EntryInfo {
            name: entry.name.clone(),
            method: entry.method,
            size: Some(entry.size),
            compressed_size: Some(entry.compressed_size),
        };
        self.active = Some(Active::new(
            info.clone(),
            entry.flags,
            entry.crc,
            false,
            true,
        )?);
        Ok(Some(info))
    }
}

/// Compression for an entry written with [`ZipWriter::add`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZipMethod {
    Stored,
    Deflate,
}

struct Record {
    name: String,
    method: u16,
    flags: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
}

struct Streaming {
    crc: crc32fast::Hasher,
    size: u64,
}

/// Writes a zip archive to any [`Write`]. Entries can be added whole
/// ([`add`](Self::add)) or streamed ([`start_entry`](Self::start_entry),
/// then `write`, then [`finish_entry`](Self::finish_entry)).
pub struct ZipWriter<W: Write> {
    out: W,
    offset: u64,
    records: Vec<Record>,
    current: Option<Streaming>,
    time: u16,
    date: u16,
}

impl<W: Write> ZipWriter<W> {
    pub fn new(out: W) -> Self {
        let (time, date) = dos_now();
        Self {
            out,
            offset: 0,
            records: Vec::new(),
            current: None,
            time,
            date,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    fn local_header(&mut self, record: &Record, zip64_extra: bool) -> io::Result<()> {
        let mut h = Vec::with_capacity(30 + record.name.len() + 20);
        h.extend_from_slice(&LOCAL_HEADER.to_le_bytes());
        h.extend_from_slice(&(if zip64_extra { 45u16 } else { 20u16 }).to_le_bytes());
        h.extend_from_slice(&record.flags.to_le_bytes());
        h.extend_from_slice(&record.method.to_le_bytes());
        h.extend_from_slice(&self.time.to_le_bytes());
        h.extend_from_slice(&self.date.to_le_bytes());
        h.extend_from_slice(&record.crc.to_le_bytes());
        let (c, s) = if zip64_extra {
            (u32::MAX, u32::MAX)
        } else {
            (record.compressed as u32, record.size as u32)
        };
        h.extend_from_slice(&c.to_le_bytes());
        h.extend_from_slice(&s.to_le_bytes());
        h.extend_from_slice(&(record.name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(if zip64_extra { 20u16 } else { 0u16 }).to_le_bytes());
        h.extend_from_slice(record.name.as_bytes());
        if zip64_extra {
            h.extend_from_slice(&1u16.to_le_bytes());
            h.extend_from_slice(&16u16.to_le_bytes());
            h.extend_from_slice(&record.size.to_le_bytes());
            h.extend_from_slice(&record.compressed.to_le_bytes());
        }
        self.put(&h)
    }

    /// Add a whole entry whose bytes are already in hand.
    pub fn add(&mut self, name: &str, method: ZipMethod, data: &[u8]) -> io::Result<()> {
        assert!(self.current.is_none(), "finish the streamed entry first");
        let crc = crc32fast::hash(data);
        let body;
        let payload: &[u8] = match method {
            ZipMethod::Stored => data,
            ZipMethod::Deflate => {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(data)?;
                body = encoder.finish()?;
                &body
            }
        };
        let record = Record {
            name: name.to_owned(),
            method: if method == ZipMethod::Stored {
                METHOD_STORED
            } else {
                METHOD_DEFLATE
            },
            flags: FLAG_UTF8,
            crc,
            compressed: payload.len() as u64,
            size: data.len() as u64,
            offset: self.offset,
        };
        let big = record.size >= u32::MAX as u64 || record.compressed >= u32::MAX as u64;
        self.local_header(&record, big)?;
        self.put(payload)?;
        self.records.push(record);
        Ok(())
    }

    /// Begin a STORED entry whose size is not known in advance.
    pub fn start_entry(&mut self, name: &str) -> io::Result<()> {
        assert!(self.current.is_none(), "finish the streamed entry first");
        let record = Record {
            name: name.to_owned(),
            method: METHOD_STORED,
            flags: FLAG_UTF8 | FLAG_DESCRIPTOR,
            crc: 0,
            compressed: 0,
            size: 0,
            offset: self.offset,
        };
        // A zip64 extra in the local header tells streaming readers that the
        // descriptor carries 8-byte sizes, so entries may exceed 4 GiB.
        self.local_header(&record, true)?;
        self.records.push(record);
        self.current = Some(Streaming {
            crc: crc32fast::Hasher::new(),
            size: 0,
        });
        Ok(())
    }

    pub fn finish_entry(&mut self) -> io::Result<()> {
        let streaming = self.current.take().expect("no streamed entry to finish");
        let crc = streaming.crc.finalize();
        let mut d = Vec::with_capacity(24);
        d.extend_from_slice(&DESCRIPTOR.to_le_bytes());
        d.extend_from_slice(&crc.to_le_bytes());
        d.extend_from_slice(&streaming.size.to_le_bytes());
        d.extend_from_slice(&streaming.size.to_le_bytes());
        self.put(&d)?;
        let record = self.records.last_mut().unwrap();
        record.crc = crc;
        record.compressed = streaming.size;
        record.size = streaming.size;
        Ok(())
    }

    /// Write the central directory and return the underlying writer.
    pub fn finish(mut self) -> io::Result<W> {
        if self.current.is_some() {
            self.finish_entry()?;
        }
        let cd_start = self.offset;
        let records = std::mem::take(&mut self.records);
        for r in &records {
            let needs64 = r.size >= u32::MAX as u64
                || r.compressed >= u32::MAX as u64
                || r.offset >= u32::MAX as u64;
            let mut extra = Vec::new();
            if needs64 {
                extra.extend_from_slice(&1u16.to_le_bytes());
                extra.extend_from_slice(&24u16.to_le_bytes());
                extra.extend_from_slice(&r.size.to_le_bytes());
                extra.extend_from_slice(&r.compressed.to_le_bytes());
                extra.extend_from_slice(&r.offset.to_le_bytes());
            }
            let clamp = |v: u64| if needs64 { u32::MAX } else { v as u32 };
            let mut h = Vec::with_capacity(46 + r.name.len() + extra.len());
            h.extend_from_slice(&CENTRAL_HEADER.to_le_bytes());
            h.extend_from_slice(&0x031Eu16.to_le_bytes()); // made by: Unix, 3.0
            h.extend_from_slice(
                &(if needs64 || r.flags & FLAG_DESCRIPTOR != 0 {
                    45u16
                } else {
                    20u16
                })
                .to_le_bytes(),
            );
            h.extend_from_slice(&r.flags.to_le_bytes());
            h.extend_from_slice(&r.method.to_le_bytes());
            h.extend_from_slice(&self.time.to_le_bytes());
            h.extend_from_slice(&self.date.to_le_bytes());
            h.extend_from_slice(&r.crc.to_le_bytes());
            h.extend_from_slice(&clamp(r.compressed).to_le_bytes());
            h.extend_from_slice(&clamp(r.size).to_le_bytes());
            h.extend_from_slice(&(r.name.len() as u16).to_le_bytes());
            h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // comment
            h.extend_from_slice(&0u16.to_le_bytes()); // disk
            h.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            let mode: u32 = if r.name.ends_with('/') {
                0o40755
            } else {
                0o100644
            };
            h.extend_from_slice(&(mode << 16).to_le_bytes());
            h.extend_from_slice(&clamp(r.offset).to_le_bytes());
            h.extend_from_slice(r.name.as_bytes());
            h.extend_from_slice(&extra);
            self.put(&h)?;
        }
        let cd_size = self.offset - cd_start;
        let count = records.len() as u64;
        if count >= 0xFFFF || cd_size >= u32::MAX as u64 || cd_start >= u32::MAX as u64 {
            let zip64_end = self.offset;
            let mut z = Vec::with_capacity(76);
            z.extend_from_slice(&ZIP64_END.to_le_bytes());
            z.extend_from_slice(&44u64.to_le_bytes());
            z.extend_from_slice(&45u16.to_le_bytes());
            z.extend_from_slice(&45u16.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&count.to_le_bytes());
            z.extend_from_slice(&count.to_le_bytes());
            z.extend_from_slice(&cd_size.to_le_bytes());
            z.extend_from_slice(&cd_start.to_le_bytes());
            z.extend_from_slice(&ZIP64_LOCATOR.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&zip64_end.to_le_bytes());
            z.extend_from_slice(&1u32.to_le_bytes());
            self.put(&z)?;
        }
        let mut e = Vec::with_capacity(22);
        e.extend_from_slice(&END_OF_CENTRAL.to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        e.extend_from_slice(&(count.min(0xFFFF) as u16).to_le_bytes());
        e.extend_from_slice(&(count.min(0xFFFF) as u16).to_le_bytes());
        e.extend_from_slice(&(cd_size.min(u32::MAX as u64) as u32).to_le_bytes());
        e.extend_from_slice(&(cd_start.min(u32::MAX as u64) as u32).to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        self.put(&e)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for ZipWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let streaming = self
            .current
            .as_mut()
            .ok_or_else(|| io::Error::other("no zip entry is open for writing"))?;
        streaming.crc.update(buf);
        streaming.size += buf.len() as u64;
        self.out.write_all(buf)?;
        self.offset += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// The current UTC time as an MS-DOS (time, date) pair.
/// The (year, month, day) of a count of days since 1970-01-01, using
/// Howard Hinnant's civil-from-days algorithm.
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn dos_now() -> (u16, u16) {
    let secs = web_time::SystemTime::now()
        .duration_since(web_time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    if !(1980..=2107).contains(&year) {
        return (0, 0x21);
    }
    let time = (((rem / 3600) << 11) | (((rem % 3600) / 60) << 5) | ((rem % 60) / 2)) as u16;
    let date = (((year - 1980) << 9) | (month << 5) | day) as u16;
    (time, date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_all(entries: &mut impl Entries) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(info) = entries.next_entry().unwrap() {
            let mut data = Vec::new();
            entries.read_to_end(&mut data).unwrap();
            out.push((info.name, data));
        }
        out
    }

    fn payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn round_trips_whole_entries() {
        let mut w = ZipWriter::new(Vec::new());
        w.add("a/", ZipMethod::Stored, b"").unwrap();
        w.add("a/one.bin", ZipMethod::Stored, &payload(1000))
            .unwrap();
        w.add("a/two.bin", ZipMethod::Deflate, &payload(300_000))
            .unwrap();
        let bytes = w.finish().unwrap();

        let streamed = read_all(&mut StreamReader::new(Cursor::new(&bytes)));
        let indexed = read_all(&mut IndexedReader::new(Cursor::new(&bytes)).unwrap());
        for got in [streamed, indexed] {
            assert_eq!(got.len(), 3);
            assert_eq!(got[1].1, payload(1000));
            assert_eq!(got[2].1, payload(300_000));
        }
    }

    #[test]
    fn streamed_entries_with_descriptors_round_trip() {
        let mut w = ZipWriter::new(Vec::new());
        w.start_entry("x.csv").unwrap();
        // Include a fake descriptor signature inside the data.
        let mut data = payload(700_000);
        data[1234..1238].copy_from_slice(&DESCRIPTOR_BYTES);
        w.write_all(&data).unwrap();
        w.finish_entry().unwrap();
        w.start_entry("y.csv").unwrap();
        w.write_all(b"second").unwrap();
        let bytes = w.finish().unwrap();

        let got = read_all(&mut StreamReader::new(Cursor::new(&bytes)));
        assert_eq!(got[0].1, data);
        assert_eq!(got[1].1, b"second");
        let got = read_all(&mut IndexedReader::new(Cursor::new(&bytes)).unwrap());
        assert_eq!(got[0].1, data);
    }

    #[test]
    fn detects_corruption_and_truncation() {
        let mut w = ZipWriter::new(Vec::new());
        w.add("a.bin", ZipMethod::Stored, &payload(5000)).unwrap();
        let mut bytes = w.finish().unwrap();
        bytes[100] ^= 0xFF;
        let mut reader = StreamReader::new(Cursor::new(&bytes));
        reader.next_entry().unwrap();
        let err = reader.read_to_end(&mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("CRC"), "{err}");

        let mut reader = StreamReader::new(Cursor::new(&bytes[..2000]));
        reader.next_entry().unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
    }
}
