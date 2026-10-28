//! Write-ahead log for documents sitting in the RAM buffer.
//!
//! Record: `len: u32 | crc32(payload): u32 | payload`, payload = `doc_id: u64 LE | utf8 text`.
//! The log only ever covers the current buffer: a successful flush (segment
//! on disk + manifest updated) empties it. A torn or corrupt tail, which is
//! what a crash mid-append leaves behind, is cut off at the last good record
//! on replay.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::codec::crc32;
use crate::error::Result;
use crate::index::DocId;

pub struct Wal {
    file: File,
    fsync: bool,
}

impl Wal {
    /// Opens (creating if needed) and returns every intact record.
    pub fn open(path: &Path, fsync: bool) -> Result<(Wal, Vec<(DocId, String)>)> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)?;

        let mut recs = Vec::new();
        let mut at = 0usize;
        while let Some(head) = raw.get(at..at + 8) {
            let len = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(head[4..].try_into().unwrap());
            let Some(payload) = len.checked_add(at + 8).and_then(|end| raw.get(at + 8..end)) else { break };
            if len < 8 || crc32::checksum(payload) != crc {
                break;
            }
            let Ok(text) = std::str::from_utf8(&payload[8..]) else { break };
            recs.push((u64::from_le_bytes(payload[..8].try_into().unwrap()), text.to_string()));
            at += 8 + len;
        }

        if at < raw.len() {
            file.set_len(at as u64)?;
        }
        file.seek(SeekFrom::Start(at as u64))?;
        Ok((Wal { file, fsync }, recs))
    }

    pub fn append(&mut self, id: DocId, text: &str) -> Result<()> {
        self.append_batch(&[(id, text)])
    }

    // one write and (optionally) one fsync for the whole batch. A crash can
    // tear the batch, replay then keeps its intact prefix
    pub fn append_batch(&mut self, docs: &[(DocId, &str)]) -> Result<()> {
        let mut out = Vec::with_capacity(docs.iter().map(|d| 16 + d.1.len()).sum());
        for &(id, text) in docs {
            let start = out.len();
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(text.as_bytes());
            let payload = &out[start + 8..];
            let (len, crc) = (payload.len() as u32, crc32::checksum(payload));
            out[start..start + 4].copy_from_slice(&len.to_le_bytes());
            out[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
        }
        self.file.write_all(&out)?;
        if self.fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }

    pub fn reset(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        if self.fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("hiis-wal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn append_replay_reset() {
        let p = tmp("a");
        {
            let (mut w, old) = Wal::open(&p, false).unwrap();
            assert!(old.is_empty());
            w.append(7, "hello").unwrap();
            w.append(8, "عربي").unwrap();
        }
        let (mut w, recs) = Wal::open(&p, false).unwrap();
        assert_eq!(recs, [(7, "hello".to_string()), (8, "عربي".to_string())]);
        w.append(9, "more").unwrap();
        w.reset().unwrap();
        drop(w);
        assert!(Wal::open(&p, false).unwrap().1.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn batch_is_replayed_in_order() {
        let p = tmp("d");
        {
            let (mut w, _) = Wal::open(&p, false).unwrap();
            w.append_batch(&[(1, "one"), (2, ""), (3, "three")]).unwrap();
            w.append_batch(&[]).unwrap();
        }
        let recs = Wal::open(&p, false).unwrap().1;
        assert_eq!(recs, [(1, "one".to_string()), (2, String::new()), (3, "three".to_string())]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn torn_tail_is_cut_off() {
        let p = tmp("b");
        {
            let (mut w, _) = Wal::open(&p, false).unwrap();
            w.append(1, "good").unwrap();
            w.append(2, "also good").unwrap();
        }
        let mut raw = std::fs::read(&p).unwrap();
        let good_len = raw.len();
        raw.extend_from_slice(&[40, 0, 0, 0, 1, 2, 3]); // half a header + junk
        std::fs::write(&p, &raw).unwrap();

        let (mut w, recs) = Wal::open(&p, false).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(std::fs::metadata(&p).unwrap().len() as usize, good_len);
        // appends continue cleanly after the cut
        w.append(3, "after").unwrap();
        drop(w);
        assert_eq!(Wal::open(&p, false).unwrap().1.len(), 3);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn flipped_byte_stops_replay_there() {
        let p = tmp("c");
        {
            let (mut w, _) = Wal::open(&p, false).unwrap();
            w.append(1, "first").unwrap();
            w.append(2, "second").unwrap();
        }
        let mut raw = std::fs::read(&p).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        std::fs::write(&p, raw).unwrap();
        assert_eq!(Wal::open(&p, false).unwrap().1.len(), 1);
        let _ = std::fs::remove_file(&p);
    }
}
