//! Parallel gzip, the way pigz does it. The input is cut into chunks that
//! worker threads compress as raw deflate, each primed with the 32 KiB before
//! it and ended with a sync flush, so the pieces join into one ordinary gzip
//! member. The output depends only on the input and the level, never on the
//! number of threads.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use flate2::{Compress, Compression, Crc, FlushCompress};

/// Bytes compressed per job.
const CHUNK: usize = 1 << 20;
/// Deflate's window: how much of the previous chunk primes the next.
const WINDOW: usize = 32 << 10;
/// Fixed header: no name or comment, mtime 0, unknown OS, as flate2 writes.
const HEADER: [u8; 10] = [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];

struct Job {
    index: u64,
    data: Vec<u8>,
    dict: Vec<u8>,
}

struct Done {
    index: u64,
    out: Vec<u8>,
    crc: Crc,
}

struct Workers {
    jobs: SyncSender<Job>,
    results: Receiver<io::Result<Done>>,
    handles: Vec<JoinHandle<()>>,
}

/// A gzip encoder that compresses on `threads` threads, started once the
/// input outgrows one chunk.
pub struct ParGzEncoder<W: Write> {
    /// `None` only after `finish`.
    inner: Option<W>,
    level: Compression,
    threads: usize,
    workers: Option<Workers>,
    buf: Vec<u8>,
    dict: Vec<u8>,
    submitted: u64,
    written: u64,
    ready: BTreeMap<u64, Done>,
    crc: Crc,
}

impl<W: Write> ParGzEncoder<W> {
    pub fn new(inner: W, level: Compression, threads: usize) -> Self {
        Self {
            inner: Some(inner),
            level,
            threads: threads.max(1),
            workers: None,
            buf: Vec::with_capacity(CHUNK),
            dict: Vec::new(),
            submitted: 0,
            written: 0,
            ready: BTreeMap::new(),
            crc: Crc::new(),
        }
    }

    /// Writes the rest of the stream and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        if self.submitted == 0 {
            // Fits in one chunk: no threads, same bytes.
            let done = compress(0, self.level, std::mem::take(&mut self.buf), Vec::new())?;
            self.inner().write_all(&HEADER)?;
            self.emit(done)?;
        } else {
            if !self.buf.is_empty() {
                self.submit()?;
            }
            while self.written < self.submitted {
                self.receive()?;
            }
        }
        let mut last = Vec::with_capacity(16);
        Compress::new(self.level, false)
            .compress_vec(&[], &mut last, FlushCompress::Finish)
            .map_err(io::Error::other)?;
        let (sum, amount) = (self.crc.sum(), self.crc.amount());
        let inner = self.inner();
        inner.write_all(&last)?;
        inner.write_all(&sum.to_le_bytes())?;
        inner.write_all(&amount.to_le_bytes())?;
        inner.flush()?;
        self.stop();
        Ok(self.inner.take().expect("finished once"))
    }

    fn inner(&mut self) -> &mut W {
        self.inner.as_mut().expect("used after finish")
    }

    fn submit(&mut self) -> io::Result<()> {
        if self.workers.is_none() {
            self.workers = Some(self.start());
            self.inner().write_all(&HEADER)?;
        }
        // Bound the memory held in flight.
        while self.submitted - self.written >= 2 * self.threads as u64 {
            self.receive()?;
        }
        let data = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        let dict = std::mem::replace(
            &mut self.dict,
            data[data.len().saturating_sub(WINDOW)..].to_vec(),
        );
        let job = Job {
            index: self.submitted,
            data,
            dict,
        };
        let workers = self.workers.as_ref().expect("started above");
        workers
            .jobs
            .send(job)
            .map_err(|_| io::Error::other("gzip workers stopped"))?;
        self.submitted += 1;
        Ok(())
    }

    /// Waits for one compressed chunk and writes every chunk now in order.
    fn receive(&mut self) -> io::Result<()> {
        let workers = self.workers.as_ref().expect("receive after submit");
        let done = workers
            .results
            .recv()
            .map_err(|_| io::Error::other("gzip workers stopped"))??;
        self.ready.insert(done.index, done);
        while let Some(done) = self.ready.remove(&self.written) {
            self.emit(done)?;
        }
        Ok(())
    }

    fn emit(&mut self, done: Done) -> io::Result<()> {
        self.inner().write_all(&done.out)?;
        self.crc.combine(&done.crc);
        self.written += 1;
        Ok(())
    }

    fn start(&self) -> Workers {
        let (jobs, job_rx) = sync_channel::<Job>(self.threads);
        let (result_tx, results) = sync_channel(2 * self.threads);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let level = self.level;
        let handles = (0..self.threads)
            .map(|_| {
                let job_rx = Arc::clone(&job_rx);
                let result_tx = result_tx.clone();
                std::thread::spawn(move || {
                    loop {
                        let job = match job_rx.lock() {
                            Ok(rx) => rx.recv(),
                            Err(_) => return,
                        };
                        let Ok(Job { index, data, dict }) = job else {
                            return;
                        };
                        let done =
                            catch_unwind(AssertUnwindSafe(|| compress(index, level, data, dict)))
                                .unwrap_or_else(|_| Err(io::Error::other("gzip worker panicked")));
                        if result_tx.send(done).is_err() {
                            return;
                        }
                    }
                })
            })
            .collect();
        Workers {
            jobs,
            results,
            handles,
        }
    }

    fn stop(&mut self) {
        if let Some(Workers {
            jobs,
            results,
            handles,
        }) = self.workers.take()
        {
            drop(jobs);
            drop(results);
            for handle in handles {
                let _ = handle.join();
            }
        }
    }
}

impl<W: Write> Write for ParGzEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(CHUNK - self.buf.len());
        self.buf.extend_from_slice(&buf[..n]);
        if self.buf.len() == CHUNK {
            self.submit()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner().flush()
    }
}

impl<W: Write> Drop for ParGzEncoder<W> {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Raw deflate of `data`, primed with `dict` and ended with a sync flush so
/// the next chunk can follow on a byte boundary.
fn compress(index: u64, level: Compression, data: Vec<u8>, dict: Vec<u8>) -> io::Result<Done> {
    let mut c = Compress::new(level, false);
    if !dict.is_empty() {
        c.set_dictionary(&dict).map_err(io::Error::other)?;
    }
    let mut out = Vec::with_capacity(data.len() + data.len() / 16 + 64);
    loop {
        if out.capacity() - out.len() < 64 {
            out.reserve(out.capacity());
        }
        let consumed = usize::try_from(c.total_in()).map_err(io::Error::other)?;
        c.compress_vec(&data[consumed..], &mut out, FlushCompress::Sync)
            .map_err(io::Error::other)?;
        // zlib's flush is complete once it stops filling the output.
        if c.total_in() == data.len() as u64 && out.len() < out.capacity() {
            break;
        }
    }
    let mut crc = Crc::new();
    crc.update(&data);
    Ok(Done { index, out, crc })
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::GzDecoder;

    use super::*;

    fn encode(data: &[u8], threads: usize) -> Vec<u8> {
        let mut enc = ParGzEncoder::new(Vec::new(), Compression::default(), threads);
        // Uneven writes cross chunk boundaries mid-buffer.
        for piece in data.chunks(77_777) {
            enc.write_all(piece).unwrap();
        }
        enc.finish().unwrap()
    }

    fn sample(len: usize) -> Vec<u8> {
        // Half text-like (compressible), half pseudo-random.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..len)
            .map(|i| {
                if (i / 4096) % 2 == 0 {
                    b"layer contents repeat "[i % 22]
                } else {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                }
            })
            .collect()
    }

    #[test]
    fn output_is_one_gzip_member_that_decodes_to_the_input() {
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17] {
            let data = sample(len);
            let gz = encode(&data, 3);
            // GzDecoder stops after the first member, so a full decode
            // proves the chunks joined into one.
            let mut back = Vec::new();
            GzDecoder::new(&gz[..]).read_to_end(&mut back).unwrap();
            assert_eq!(back, data, "len {len}");
        }
    }

    #[test]
    fn output_does_not_depend_on_the_thread_count() {
        let data = sample(3 * CHUNK + 17);
        assert_eq!(encode(&data, 1), encode(&data, 4));
    }

    #[test]
    fn chunks_are_primed_with_the_previous_window() {
        // A random 16 KiB block, repeated. An unprimed second chunk would pay
        // for the block again, ~16 KiB more than serial gzip.
        let mut x: u64 = 1;
        let block: Vec<u8> = (0..WINDOW / 2)
            .map(|_| {
                x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                (x >> 56) as u8
            })
            .collect();
        let data: Vec<u8> = block.iter().copied().cycle().take(2 * CHUNK).collect();
        let mut serial = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        serial.write_all(&data).unwrap();
        let serial = serial.finish().unwrap().len();
        let parallel = encode(&data, 2).len();
        assert!(
            parallel < serial + 1024,
            "parallel {parallel}, serial {serial}"
        );
    }
}
