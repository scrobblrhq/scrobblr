//! A ZIP archive written front to back, for streaming: entries are deflated
//! as written and followed by a data descriptor. ZIP64 records only where a
//! size, offset or entry count needs them, as Go and Java write them. The
//! caller drains the output with [`ZipWriter::take`].

use std::io::Write;

use chrono::{DateTime, Datelike, Timelike, Utc};
use flate2::{Compression, write::DeflateEncoder};

const LOCAL_HEADER: u32 = 0x0403_4b50;
const DATA_DESCRIPTOR: u32 = 0x0807_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;
const ZIP64_END_OF_CENTRAL_DIRECTORY: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_EXTRA: u16 = 0x0001;

const FLAG_DESCRIPTOR: u16 = 1 << 3;
const FLAG_UTF8: u16 = 1 << 11;
const STORED: u16 = 0;
const DEFLATED: u16 = 8;
const VERSION: u16 = 20;
const VERSION_ZIP64: u16 = 45;
/// Unix, so readers take the mode from the external attributes.
const MADE_BY: u16 = (3 << 8) | 63;
const FILE_MODE: u32 = 0o100_644 << 16;
const U32_MAX: u64 = u32::MAX as u64;

struct Entry {
    name: String,
    flags: u16,
    method: u16,
    time: u16,
    date: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
}

impl Entry {
    fn zip64(&self) -> bool {
        self.compressed >= U32_MAX || self.size >= U32_MAX || self.offset >= U32_MAX
    }
}

struct Open {
    entry: Entry,
    encoder: DeflateEncoder<Vec<u8>>,
    crc: crc32fast::Hasher,
}

pub struct ZipWriter {
    out: Vec<u8>,
    /// Bytes produced so far, taken or not.
    offset: u64,
    entries: Vec<Entry>,
    open: Option<Open>,
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipWriter {
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            offset: 0,
            entries: Vec::new(),
            open: None,
        }
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    pub fn buffered(&self) -> usize {
        self.out.len()
    }

    /// Starts a deflated entry, written in pieces with [`write`](Self::write).
    pub fn start(&mut self, name: &str, modified: DateTime<Utc>) {
        assert!(self.open.is_none(), "an entry is already open");
        let (time, date) = dos_time(modified);
        let entry = Entry {
            name: name.to_string(),
            flags: FLAG_DESCRIPTOR | FLAG_UTF8,
            method: DEFLATED,
            time,
            date,
            crc: 0,
            compressed: 0,
            size: 0,
            offset: self.offset,
        };
        self.local_header(&entry);
        self.open = Some(Open {
            entry,
            encoder: DeflateEncoder::new(Vec::new(), Compression::new(6)),
            crc: crc32fast::Hasher::new(),
        });
    }

    pub fn write(&mut self, data: &[u8]) {
        let open = self.open.as_mut().expect("no entry is open");
        open.crc.update(data);
        open.entry.size += data.len() as u64;
        open.encoder
            .write_all(data)
            .expect("deflating into memory cannot fail");
        let compressed = std::mem::take(open.encoder.get_mut());
        open.entry.compressed += compressed.len() as u64;
        self.emit(&compressed);
    }

    /// Ends the open entry with its data descriptor.
    pub fn finish_entry(&mut self) {
        let Open {
            mut entry,
            encoder,
            crc,
        } = self.open.take().expect("no entry is open");
        let rest = encoder.finish().expect("deflating into memory cannot fail");
        entry.compressed += rest.len() as u64;
        entry.crc = crc.finalize();
        self.emit(&rest);

        let mut d = Vec::with_capacity(24);
        put32(&mut d, DATA_DESCRIPTOR);
        put32(&mut d, entry.crc);
        if entry.compressed >= U32_MAX || entry.size >= U32_MAX {
            put64(&mut d, entry.compressed);
            put64(&mut d, entry.size);
        } else {
            put32(&mut d, entry.compressed as u32);
            put32(&mut d, entry.size as u32);
        }
        self.emit(&d);
        self.entries.push(entry);
    }

    /// Adds a file as it is (already compressed, such as a JPEG).
    pub fn add_stored(&mut self, name: &str, modified: DateTime<Utc>, data: &[u8]) {
        assert!(self.open.is_none(), "an entry is already open");
        let (time, date) = dos_time(modified);
        let entry = Entry {
            name: name.to_string(),
            flags: FLAG_UTF8,
            method: STORED,
            time,
            date,
            crc: crc32fast::hash(data),
            compressed: data.len() as u64,
            size: data.len() as u64,
            offset: self.offset,
        };
        self.local_header(&entry);
        self.emit(data);
        self.entries.push(entry);
    }

    /// Writes the central directory. Nothing can be added afterwards.
    pub fn finish(&mut self) {
        assert!(self.open.is_none(), "an entry is still open");
        let start = self.offset;
        let entries = std::mem::take(&mut self.entries);
        for entry in &entries {
            let zip64 = entry.zip64();
            let mut extra = Vec::new();
            if zip64 {
                put16(&mut extra, ZIP64_EXTRA);
                put16(&mut extra, 24);
                put64(&mut extra, entry.size);
                put64(&mut extra, entry.compressed);
                put64(&mut extra, entry.offset);
            }
            let clamp = |v: u64| if zip64 { u32::MAX } else { v as u32 };
            let mut h = Vec::with_capacity(46 + entry.name.len() + extra.len());
            put32(&mut h, CENTRAL_HEADER);
            put16(&mut h, MADE_BY);
            put16(&mut h, if zip64 { VERSION_ZIP64 } else { VERSION });
            put16(&mut h, entry.flags);
            put16(&mut h, entry.method);
            put16(&mut h, entry.time);
            put16(&mut h, entry.date);
            put32(&mut h, entry.crc);
            put32(&mut h, clamp(entry.compressed));
            put32(&mut h, clamp(entry.size));
            put16(&mut h, entry.name.len() as u16);
            put16(&mut h, extra.len() as u16);
            put16(&mut h, 0); // comment
            put16(&mut h, 0); // disk
            put16(&mut h, 0); // internal attributes
            put32(&mut h, FILE_MODE);
            put32(&mut h, clamp(entry.offset));
            h.extend_from_slice(entry.name.as_bytes());
            h.extend_from_slice(&extra);
            self.emit(&h);
        }
        let size = self.offset - start;
        let count = entries.len() as u64;

        let mut e = Vec::new();
        let zip64 = count >= 0xFFFF || size >= U32_MAX || start >= U32_MAX;
        if zip64 {
            let record = self.offset;
            put32(&mut e, ZIP64_END_OF_CENTRAL_DIRECTORY);
            put64(&mut e, 44); // what follows this field
            put16(&mut e, MADE_BY);
            put16(&mut e, VERSION_ZIP64);
            put32(&mut e, 0); // this disk
            put32(&mut e, 0); // the directory's disk
            put64(&mut e, count);
            put64(&mut e, count);
            put64(&mut e, size);
            put64(&mut e, start);
            put32(&mut e, ZIP64_LOCATOR);
            put32(&mut e, 0);
            put64(&mut e, record);
            put32(&mut e, 1); // disks
        }
        put32(&mut e, END_OF_CENTRAL_DIRECTORY);
        put16(&mut e, 0);
        put16(&mut e, 0);
        put16(&mut e, count.min(0xFFFF) as u16);
        put16(&mut e, count.min(0xFFFF) as u16);
        put32(&mut e, size.min(U32_MAX) as u32);
        put32(&mut e, start.min(U32_MAX) as u32);
        put16(&mut e, 0); // comment
        self.emit(&e);
    }

    fn local_header(&mut self, entry: &Entry) {
        let streamed = entry.flags & FLAG_DESCRIPTOR != 0;
        let mut h = Vec::with_capacity(30 + entry.name.len());
        put32(&mut h, LOCAL_HEADER);
        put16(&mut h, VERSION);
        put16(&mut h, entry.flags);
        put16(&mut h, entry.method);
        put16(&mut h, entry.time);
        put16(&mut h, entry.date);
        // A streamed entry's CRC and sizes are in its data descriptor.
        let known = |v: u32| if streamed { 0 } else { v };
        put32(&mut h, known(entry.crc));
        put32(&mut h, known(entry.compressed.min(U32_MAX) as u32));
        put32(&mut h, known(entry.size.min(U32_MAX) as u32));
        put16(&mut h, entry.name.len() as u16);
        put16(&mut h, 0); // extra
        h.extend_from_slice(entry.name.as_bytes());
        self.emit(&h);
    }

    fn emit(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
        self.offset += bytes.len() as u64;
    }
}

/// MS-DOS date and time (2-second resolution, 1980 at the earliest).
fn dos_time(t: DateTime<Utc>) -> (u16, u16) {
    let year = t.year().clamp(1980, 2107) as u16;
    let time = ((t.hour() as u16) << 11) | ((t.minute() as u16) << 5) | (t.second() as u16 / 2);
    let date = ((year - 1980) << 9) | ((t.month() as u16) << 5) | t.day() as u16;
    (time, date)
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::*;

    fn read_all(bytes: Vec<u8>) -> Vec<(String, Vec<u8>)> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..archive.len())
            .map(|i| {
                let mut file = archive.by_index(i).unwrap();
                let mut content = Vec::new();
                file.read_to_end(&mut content).unwrap();
                (file.name().unwrap().to_string(), content)
            })
            .collect()
    }

    #[test]
    fn streamed_and_stored_entries_read_back() {
        let at = "2026-10-11T12:34:56Z".parse().unwrap();
        let mut zip = ZipWriter::new();
        let mut sent = Vec::new();
        zip.start("scrobbles.csv", at);
        let mut expected = Vec::new();
        for i in 0..20_000 {
            let line = format!("{i},Artist {},Track ñ {i}\n", i % 7);
            zip.write(line.as_bytes());
            expected.extend_from_slice(line.as_bytes());
            if zip.buffered() > 4096 {
                sent.extend(zip.take());
            }
        }
        zip.finish_entry();
        zip.start("empty.json", at);
        zip.finish_entry();
        zip.add_stored("uploads/avatars/ab/x.jpg", at, b"\xff\xd8jpeg");
        zip.finish();
        sent.extend(zip.take());

        let entries = read_all(sent.clone());
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0, "scrobbles.csv");
        assert_eq!(entries[0].1, expected);
        assert_eq!(entries[1], ("empty.json".into(), vec![]));
        assert_eq!(entries[2].1, b"\xff\xd8jpeg");
        // Deflated, not stored.
        assert!(sent.len() < expected.len() / 3);

        let archive = zip::ZipArchive::new(Cursor::new(sent)).unwrap();
        let modified = archive
            .clone()
            .by_index(0)
            .unwrap()
            .last_modified()
            .unwrap();
        assert_eq!(
            (modified.year(), modified.month(), modified.day()),
            (2026, 10, 11)
        );
        assert_eq!(
            (modified.hour(), modified.minute(), modified.second()),
            (12, 34, 56)
        );
    }

    #[test]
    fn many_entries_take_zip64_records() {
        let at = Utc::now();
        let mut zip = ZipWriter::new();
        for i in 0..70_000 {
            zip.add_stored(&format!("f/{i}"), at, b"x");
        }
        zip.finish();
        let bytes = zip.take();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), 70_000);
        let mut last = String::new();
        archive
            .by_name("f/69999")
            .unwrap()
            .read_to_string(&mut last)
            .unwrap();
        assert_eq!(last, "x");
    }
}
