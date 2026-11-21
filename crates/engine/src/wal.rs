//! Write-ahead log for `durability: wal` (SPEC §5.2): a request is acknowledged only after
//! its rows are fsynced. Writers hand records to one thread that drains everything queued,
//! writes it, and issues one fsync for the whole group (group commit).
//!
//! One segment per flush sequence: `<dir>/<epoch>-<seq>.wal`. A segment is deleted once its
//! envelope is in object storage; segments still present at start are replayed.
//! Record framing: `u32 len (LE) ‖ u8 table index ‖ Arrow IPC stream` — a torn tail is ignored.

use crate::tables::Table;
use anyhow::{Context, Result};
use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use tokio::sync::oneshot;

struct Req {
    seq: i64,
    frames: Vec<u8>,
    done: oneshot::Sender<std::io::Result<()>>,
}

pub struct Wal {
    dir: PathBuf,
    epoch: i64,
    tx: mpsc::Sender<Req>,
}

pub fn encode(table: Table, batch: &RecordBatch, out: &mut Vec<u8>) -> Result<()> {
    let mut ipc = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut ipc, &batch.schema())?;
        w.write(batch)?;
        w.finish()?;
    }
    let idx = Table::ALL.iter().position(|t| *t == table).expect("table in ALL") as u8;
    out.extend_from_slice(&((ipc.len() + 1) as u32).to_le_bytes());
    out.push(idx);
    out.extend_from_slice(&ipc);
    Ok(())
}

fn segment(dir: &Path, epoch: i64, seq: i64) -> PathBuf {
    dir.join(format!("{epoch}-{seq:012}.wal"))
}

impl Wal {
    pub fn open(dir: &Path, epoch: i64) -> Result<Wal> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating WAL dir {}", dir.display()))?;
        let (tx, rx) = mpsc::channel::<Req>();
        let d = dir.to_path_buf();
        std::thread::Builder::new().name("wal".into()).spawn(move || writer(d, epoch, rx))?;
        Ok(Wal { dir: dir.to_path_buf(), epoch, tx })
    }

    /// Durably append encoded frames to segment `seq`; returns after fsync.
    pub async fn append(&self, seq: i64, frames: Vec<u8>) -> Result<()> {
        let (done, wait) = oneshot::channel();
        self.tx.send(Req { seq, frames, done }).map_err(|_| anyhow::anyhow!("WAL thread stopped"))?;
        wait.await.map_err(|_| anyhow::anyhow!("WAL thread stopped"))?.context("WAL write")?;
        Ok(())
    }

    /// Forget segment `seq` of this epoch (its rows are in object storage now).
    pub fn remove(&self, seq: i64) {
        let _ = std::fs::remove_file(segment(&self.dir, self.epoch, seq));
    }

    /// Segments left by earlier processes: `(epoch, seq, path)`, oldest first.
    pub fn leftovers(dir: &Path) -> Result<Vec<(i64, i64, PathBuf)>> {
        let mut out = vec![];
        let Ok(rd) = std::fs::read_dir(dir) else { return Ok(out) };
        for e in rd {
            let p = e?.path();
            let Some(stem) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".wal")) else { continue };
            if let Some((ep, sq)) = stem.split_once('-')
                && let (Ok(ep), Ok(sq)) = (ep.parse(), sq.parse())
            {
                out.push((ep, sq, p));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Every complete record of a segment.
    pub fn read(path: &Path) -> Result<Vec<(Table, RecordBatch)>> {
        let mut buf = Vec::new();
        File::open(path)?.read_to_end(&mut buf)?;
        let mut out = vec![];
        let mut i = 0;
        while i + 4 <= buf.len() {
            let len = u32::from_le_bytes(buf[i..i + 4].try_into().expect("4 bytes")) as usize;
            if len == 0 || i + 4 + len > buf.len() {
                break; // torn tail: the request was never acknowledged
            }
            let rec = &buf[i + 4..i + 4 + len];
            let table = *Table::ALL.get(rec[0] as usize).context("WAL record with unknown table")?;
            for b in StreamReader::try_new(&rec[1..], None)? {
                out.push((table, b?));
            }
            i += 4 + len;
        }
        Ok(out)
    }
}

fn writer(dir: PathBuf, epoch: i64, rx: mpsc::Receiver<Req>) {
    let mut files: BTreeMap<i64, File> = BTreeMap::new();
    while let Ok(first) = rx.recv() {
        let mut group = vec![first];
        while let Ok(r) = rx.try_recv() {
            group.push(r);
        }
        let mut touched = vec![];
        let mut result: std::io::Result<()> = Ok(());
        for r in &group {
            if result.is_err() {
                break;
            }
            let f = match files.entry(r.seq) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(v) => {
                    match OpenOptions::new().create(true).append(true).open(segment(&dir, epoch, r.seq)) {
                        Ok(f) => v.insert(f),
                        Err(e) => {
                            result = Err(e);
                            break;
                        }
                    }
                }
            };
            result = f.write_all(&r.frames);
            if !touched.contains(&r.seq) {
                touched.push(r.seq);
            }
        }
        if result.is_ok() {
            for s in &touched {
                if let Some(f) = files.get(s)
                    && let Err(e) = f.sync_data()
                {
                    result = Err(e);
                    break;
                }
            }
        }
        // Keep only the newest two segments open.
        while files.len() > 2 {
            files.pop_first();
        }
        for r in group {
            let _ = r.done.send(match &result {
                Ok(()) => Ok(()),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            });
        }
    }
}
