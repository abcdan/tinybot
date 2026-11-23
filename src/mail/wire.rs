use std::{
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const MAX_WIRE_BYTES: usize = 4 * 1024 * 1024;
const MAX_LITERAL_BYTES: usize = 512 * 1024;
const READ_CHUNK: usize = 8192;

// Guard the IMAP decoder before it allocates buffers for server-announced literals.
#[derive(Debug)]
pub(super) struct BoundedImapStream<T> {
    inner: T,
    remaining_bytes: usize,
    remaining_literal: usize,
    prefix: Vec<u8>,
    continuation: bool,
    suffix: Suffix,
}

#[derive(Debug, Default)]
enum Suffix {
    #[default]
    None,
    Brace,
    Digits(usize),
    Closed(usize),
    CarriageReturn(usize),
}

impl<T> BoundedImapStream<T> {
    pub(super) fn new(inner: T) -> Self {
        Self {
            inner,
            remaining_bytes: MAX_WIRE_BYTES,
            remaining_literal: 0,
            prefix: Vec::with_capacity(16),
            continuation: false,
            suffix: Suffix::None,
        }
    }

    fn check_literals(&mut self, mut decoded: &[u8]) -> io::Result<()> {
        while let Some((&byte, rest)) = decoded.split_first() {
            if self.remaining_literal > 0 {
                let skipped = self.remaining_literal.min(decoded.len());
                self.remaining_literal -= skipped;
                decoded = &decoded[skipped..];
                continue;
            }
            if !self.continuation && self.prefix.len() < 16 {
                self.prefix.push(byte);
            }
            self.suffix = match (&self.suffix, byte) {
                (_, b'{') => Suffix::Brace,
                (Suffix::Brace, b'0'..=b'9') => Suffix::Digits(usize::from(byte - b'0')),
                (Suffix::Digits(size), b'0'..=b'9') => Suffix::Digits(
                    size.saturating_mul(10)
                        .saturating_add(usize::from(byte - b'0')),
                ),
                (Suffix::Digits(size), b'}') => Suffix::Closed(*size),
                (Suffix::Closed(size), b'\r') => Suffix::CarriageReturn(*size),
                (Suffix::CarriageReturn(size), b'\n') if self.data_response() => {
                    if *size > MAX_LITERAL_BYTES {
                        return Err(io::Error::other("IMAP response literal exceeds size limit"));
                    }
                    self.remaining_literal = *size;
                    self.continuation = true;
                    Suffix::None
                }
                (_, b'\n') => {
                    self.continuation = false;
                    self.prefix.clear();
                    Suffix::None
                }
                _ => Suffix::None,
            };
            decoded = rest;
        }
        Ok(())
    }

    fn data_response(&self) -> bool {
        if self.continuation {
            return true;
        }
        // Status and continuation text may end with braces without introducing a literal.
        self.prefix.strip_prefix(b"* ").is_some_and(|rest| {
            let name = rest
                .split(|byte| byte.is_ascii_whitespace())
                .next()
                .unwrap_or_default();
            [b"OK".as_slice(), b"NO", b"BAD", b"BYE", b"PREAUTH"]
                .into_iter()
                .all(|status| !name.eq_ignore_ascii_case(status))
        })
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for BoundedImapStream<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.remaining_bytes == 0 {
            return Poll::Ready(Err(io::Error::other("IMAP session exceeds size limit")));
        }
        let chunk = this.remaining_bytes.min(buffer.remaining()).min(READ_CHUNK);
        let mut incoming = ReadBuf::new(buffer.initialize_unfilled_to(chunk));
        ready!(Pin::new(&mut this.inner).poll_read(cx, &mut incoming))?;
        this.check_literals(incoming.filled())?;
        let received = incoming.filled().len();
        this.remaining_bytes -= received;
        buffer.advance(received);
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for BoundedImapStream<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_rejects_oversized_literals_before_imap_parser_receives_them() {
        let mut wire = BoundedImapStream::new(());
        assert!(wire.check_literals(b"* 1 FETCH (BODY[] {999").is_ok());
        assert!(
            wire.check_literals(b"999999999999999999999999999}\r")
                .is_ok()
        );
        assert!(wire.check_literals(b"\n").is_err());
    }

    #[test]
    fn wire_ignores_literal_markers_in_email_text() {
        let mut wire = BoundedImapStream::new(());
        assert!(
            wire.check_literals(b"* 1 FETCH (BODY[] {12}\r\nA{9999999}\r\n)")
                .is_ok()
        );
        assert_eq!(wire.remaining_literal, 0);
    }

    #[test]
    fn wire_distinguishes_status_text_from_literal_markers() {
        let mut wire = BoundedImapStream::new(());
        assert!(wire.check_literals(b"* OK ready {10000}\r\n").is_ok());
        assert!(
            wire.check_literals(b"* 1 FETCH (BODY[] {999999999}\r\n")
                .is_err()
        );
    }
}
