//! Synthetic file data: a reader that produces it and a verifier that checks
//! it.
//!
//! The keystream is addressed by byte offset, so a verifier accepts the data in
//! whatever chunk sizes the SDK hands it, and nothing is ever held in memory or
//! on disk.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use aes::Aes128;
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const COMPARE_CHUNK: usize = 1 << 20;

type Cipher = Ctr128BE<Aes128>;

/// The contents of one file. The same seed and index always produce the same
/// bytes.
pub struct Data {
    key: [u8; 16],
    size: u64,
}

pub struct Reader {
    cipher: Cipher,
    size: u64,
    pos: u64,
}

/// Compares bytes written to it, in order, against a [`Data`] file. Timing
/// starts when the verifier is created, so create it immediately before
/// requesting the download.
pub struct Verifier {
    cipher: Cipher,
    size: u64,
    start: Instant,
    first: Option<Instant>,
    written: u64,
    scratch: Vec<u8>,
}

impl Data {
    pub fn new(seed: u64, index: u32, size: u64) -> Self {
        let mut input = [0u8; 16];
        input[..8].copy_from_slice(&seed.to_le_bytes());
        input[8..].copy_from_slice(&u64::from(index).to_le_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&Sha256::digest(input)[..16]);
        Self { key, size }
    }

    pub fn reader(&self) -> Reader {
        Reader {
            cipher: keystream(&self.key),
            size: self.size,
            pos: 0,
        }
    }

    pub fn verifier(&self) -> Verifier {
        Verifier {
            cipher: keystream(&self.key),
            size: self.size,
            start: Instant::now(),
            first: None,
            written: 0,
            scratch: Vec::new(),
        }
    }
}

impl AsyncRead for Reader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let remaining = this.size - this.pos;
        if remaining == 0 {
            return Poll::Ready(Ok(()));
        }
        let n = buf
            .remaining()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        this.cipher.write_keystream(buf.initialize_unfilled_to(n));
        buf.advance(n);
        this.pos += n as u64;
        Poll::Ready(Ok(()))
    }
}

impl Verifier {
    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.first.get_or_insert_with(Instant::now);
        if self.written + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write of {} bytes at offset {} exceeds file size {}",
                    buf.len(),
                    self.written,
                    self.size
                ),
            ));
        }

        self.scratch.resize(buf.len().min(COMPARE_CHUNK), 0);
        for chunk in buf.chunks(COMPARE_CHUNK) {
            let expected = &mut self.scratch[..chunk.len()];
            self.cipher.write_keystream(expected);
            if let Some(bad) = chunk.iter().zip(&*expected).position(|(a, b)| a != b) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("data mismatch at offset {}", self.written + bad as u64),
                ));
            }
            self.written += chunk.len() as u64;
        }
        Ok(())
    }

    /// Errors unless every byte of the file has been verified.
    pub fn complete(&self) -> io::Result<()> {
        if self.written != self.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("verified {} of {} bytes", self.written, self.size),
            ));
        }
        Ok(())
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    pub fn ttfb(&self) -> Duration {
        self.first
            .map_or(Duration::ZERO, |t| t.duration_since(self.start))
    }
}

impl AsyncWrite for Verifier {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.write(buf).map(|()| buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// AES-128 in CTR mode with a zero IV and a 128-bit big-endian block counter,
/// as Go's `cipher.NewCTR` produces.
fn keystream(key: &[u8; 16]) -> Cipher {
    Cipher::new_from_slices(key, &[0u8; 16]).expect("valid AES-128 key and IV")
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    async fn read_all(data: &Data) -> Vec<u8> {
        let mut buf = Vec::new();
        data.reader().read_to_end(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn deterministic() {
        const SIZE: u64 = 3 * COMPARE_CHUNK as u64 + 12345;
        let first = read_all(&Data::new(42, 1, SIZE)).await;
        assert_eq!(first.len() as u64, SIZE);
        assert_eq!(first, read_all(&Data::new(42, 1, SIZE)).await);
        assert_ne!(first, read_all(&Data::new(42, 2, SIZE)).await, "by index");
        assert_ne!(first, read_all(&Data::new(43, 1, SIZE)).await, "by seed");
    }

    /// Pins the keystream so a seed and index keep producing the same bytes
    /// across versions of this tool.
    #[tokio::test]
    async fn golden() {
        let all = read_all(&Data::new(42, 1, 100_000)).await;
        assert_eq!(
            hex::encode(&all[..32]),
            "c09ee3b1d8969b901564cc1ced4f85da90dab4400605d92d98058933ba8f8db5"
        );
        assert_eq!(
            hex::encode(&all[65531..65547]),
            "11253d644b277a38c96ed937978f95e2"
        );
    }

    /// The SDK writes whatever chunk sizes its download pipeline produces,
    /// which never match the reader's.
    #[tokio::test]
    async fn uneven_writes() {
        let data = Data::new(9, 1, (1 << 20) + 33);
        let all = read_all(&data).await;

        let mut v = data.verifier();
        for (start, end) in [(0usize, 4096usize), (4096, 1 << 20), (1 << 20, all.len())] {
            v.write(&all[start..end]).unwrap();
        }
        v.complete().unwrap();

        assert!(v.ttfb() > Duration::ZERO);
        assert!(v.elapsed() >= v.ttfb());
    }

    #[tokio::test]
    async fn detects_corruption() {
        let data = Data::new(1, 1, 4096);
        let mut buf = read_all(&data).await;
        buf[2000] ^= 0xff;
        let err = data.verifier().write(&buf).unwrap_err();
        assert!(err.to_string().contains("offset 2000"), "{err}");

        let mut v = data.verifier();
        assert_eq!(v.ttfb(), Duration::ZERO, "no writes yet");
        v.write(&buf[..100]).unwrap();
        assert!(v.complete().is_err(), "short write");
        assert!(v.write(&buf).is_err(), "past the end");
    }
}
