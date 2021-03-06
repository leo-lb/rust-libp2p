// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Noise protocol I/O.

pub mod handshake;

use futures::ready;
use futures::prelude::*;
use log::{debug, trace};
use snow;
use std::{fmt, io, pin::Pin, ops::DerefMut, task::{Context, Poll}};

const MAX_NOISE_PKG_LEN: usize = 65535;
const MAX_WRITE_BUF_LEN: usize = 16384;
const TOTAL_BUFFER_LEN: usize = 2 * MAX_NOISE_PKG_LEN + 3 * MAX_WRITE_BUF_LEN;

/// A single `Buffer` contains multiple non-overlapping byte buffers.
struct Buffer {
    inner: Box<[u8; TOTAL_BUFFER_LEN]>
}

/// A mutable borrow of all byte buffers, backed by `Buffer`.
struct BufferBorrow<'a> {
    read: &'a mut [u8],
    read_crypto: &'a mut [u8],
    write: &'a mut [u8],
    write_crypto: &'a mut [u8]
}

impl Buffer {
    /// Create a mutable borrow by splitting the buffer slice.
    fn borrow_mut(&mut self) -> BufferBorrow<'_> {
        let (r, w) = self.inner.split_at_mut(2 * MAX_NOISE_PKG_LEN);
        let (read, read_crypto) = r.split_at_mut(MAX_NOISE_PKG_LEN);
        let (write, write_crypto) = w.split_at_mut(MAX_WRITE_BUF_LEN);
        BufferBorrow { read, read_crypto, write, write_crypto }
    }
}

/// A passthrough enum for the two kinds of state machines in `snow`
pub(crate) enum SnowState {
    Transport(snow::TransportState),
    Handshake(snow::HandshakeState)
}

impl SnowState {
    pub fn read_message(&mut self, message: &[u8], payload: &mut [u8]) -> Result<usize, snow::Error> {
        match self {
            SnowState::Handshake(session) => session.read_message(message, payload),
            SnowState::Transport(session) => session.read_message(message, payload),
        }
    }

    pub fn write_message(&mut self, message: &[u8], payload: &mut [u8]) -> Result<usize, snow::Error> {
        match self {
            SnowState::Handshake(session) => session.write_message(message, payload),
            SnowState::Transport(session) => session.write_message(message, payload),
        }
    }

    pub fn get_remote_static(&self) -> Option<&[u8]> {
        match self {
            SnowState::Handshake(session) => session.get_remote_static(),
            SnowState::Transport(session) => session.get_remote_static(),
        }
    }

    pub fn into_transport_mode(self) -> Result<snow::TransportState, snow::Error> {
        match self {
            SnowState::Handshake(session) => session.into_transport_mode(),
            SnowState::Transport(_) => Err(snow::Error::State(snow::error::StateProblem::HandshakeAlreadyFinished)),
        }
    }
}

/// A noise session to a remote.
///
/// `T` is the type of the underlying I/O resource.
pub struct NoiseOutput<T> {
    io: T,
    session: SnowState,
    buffer: Buffer,
    read_state: ReadState,
    write_state: WriteState
}

impl<T> fmt::Debug for NoiseOutput<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NoiseOutput")
            .field("read_state", &self.read_state)
            .field("write_state", &self.write_state)
            .finish()
    }
}

impl<T> NoiseOutput<T> {
    fn new(io: T, session: SnowState) -> Self {
        NoiseOutput {
            io,
            session,
            buffer: Buffer { inner: Box::new([0; TOTAL_BUFFER_LEN]) },
            read_state: ReadState::Init,
            write_state: WriteState::Init
        }
    }
}

/// The various states of reading a noise session transitions through.
#[derive(Debug)]
enum ReadState {
    /// initial state
    Init,
    /// read frame length
    ReadLen { buf: [u8; 2], off: usize },
    /// read encrypted frame data
    ReadData { len: usize, off: usize },
    /// copy decrypted frame data
    CopyData { len: usize, off: usize },
    /// end of file has been reached (terminal state)
    /// The associated result signals if the EOF was unexpected or not.
    Eof(Result<(), ()>),
    /// decryption error (terminal state)
    DecErr
}

/// The various states of writing a noise session transitions through.
#[derive(Debug)]
enum WriteState {
    /// initial state
    Init,
    /// accumulate write data
    BufferData { off: usize },
    /// write frame length
    WriteLen { len: usize, buf: [u8; 2], off: usize },
    /// write out encrypted data
    WriteData { len: usize, off: usize },
    /// end of file has been reached (terminal state)
    Eof,
    /// encryption error (terminal state)
    EncErr
}

impl<T: AsyncRead + Unpin> AsyncRead for NoiseOutput<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let mut this = self.deref_mut();

        let buffer = this.buffer.borrow_mut();

        loop {
            trace!("read state: {:?}", this.read_state);
            match this.read_state {
                ReadState::Init => {
                    this.read_state = ReadState::ReadLen { buf: [0, 0], off: 0 };
                }
                ReadState::ReadLen { mut buf, mut off } => {
                    let n = match read_frame_len(&mut this.io, cx, &mut buf, &mut off) {
                        Poll::Ready(Ok(Some(n))) => n,
                        Poll::Ready(Ok(None)) => {
                            trace!("read: eof");
                            this.read_state = ReadState::Eof(Ok(()));
                            return Poll::Ready(Ok(0))
                        }
                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(e))
                        }
                        Poll::Pending => {
                            this.read_state = ReadState::ReadLen { buf, off };

                            return Poll::Pending;
                        }
                    };
                    trace!("read: next frame len = {}", n);
                    if n == 0 {
                        trace!("read: empty frame");
                        this.read_state = ReadState::Init;
                        continue
                    }
                    this.read_state = ReadState::ReadData { len: usize::from(n), off: 0 }
                }
                ReadState::ReadData { len, ref mut off } => {
                    let n = match ready!(
                        Pin::new(&mut this.io).poll_read(cx, &mut buffer.read[*off ..len])
                    ) {
                        Ok(n) => n,
                        Err(e) => return Poll::Ready(Err(e)),
                    };

                    trace!("read: read {}/{} bytes", *off + n, len);
                    if n == 0 {
                        trace!("read: eof");
                        this.read_state = ReadState::Eof(Err(()));
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
                    }

                    *off += n;
                    if len == *off {
                        trace!("read: decrypting {} bytes", len);
                        if let Ok(n) = this.session.read_message(
                            &buffer.read[.. len],
                            buffer.read_crypto
                        ){
                            trace!("read: payload len = {} bytes", n);
                            this.read_state = ReadState::CopyData { len: n, off: 0 }
                        } else {
                            debug!("decryption error");
                            this.read_state = ReadState::DecErr;
                            return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
                        }
                    }
                }
                ReadState::CopyData { len, ref mut off } => {
                    let n = std::cmp::min(len - *off, buf.len());
                    buf[.. n].copy_from_slice(&buffer.read_crypto[*off .. *off + n]);
                    trace!("read: copied {}/{} bytes", *off + n, len);
                    *off += n;
                    if len == *off {
                        this.read_state = ReadState::ReadLen { buf: [0, 0], off: 0 };
                    }
                    return Poll::Ready(Ok(n))
                }
                ReadState::Eof(Ok(())) => {
                    trace!("read: eof");
                    return Poll::Ready(Ok(0))
                }
                ReadState::Eof(Err(())) => {
                    trace!("read: eof (unexpected)");
                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
                }
                ReadState::DecErr => return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
            }
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for NoiseOutput<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>>{
        let mut this = self.deref_mut();

        let buffer = this.buffer.borrow_mut();

        loop {
            trace!("write state: {:?}", this.write_state);
            match this.write_state {
                WriteState::Init => {
                    this.write_state = WriteState::BufferData { off: 0 }
                }
                WriteState::BufferData { ref mut off } => {
                    let n = std::cmp::min(MAX_WRITE_BUF_LEN - *off, buf.len());
                    buffer.write[*off .. *off + n].copy_from_slice(&buf[.. n]);
                    trace!("write: buffered {} bytes", *off + n);
                    *off += n;
                    if *off == MAX_WRITE_BUF_LEN {
                        trace!("write: encrypting {} bytes", *off);
                        match this.session.write_message(buffer.write, buffer.write_crypto) {
                            Ok(n) => {
                                trace!("write: cipher text len = {} bytes", n);
                                this.write_state = WriteState::WriteLen {
                                    len: n,
                                    buf: u16::to_be_bytes(n as u16),
                                    off: 0
                                }
                            }
                            Err(e) => {
                                debug!("encryption error: {:?}", e);
                                this.write_state = WriteState::EncErr;
                                return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
                            }
                        }
                    }
                    return Poll::Ready(Ok(n))
                }
                WriteState::WriteLen { len, mut buf, mut off } => {
                    trace!("write: writing len ({}, {:?}, {}/2)", len, buf, off);
                    match write_frame_len(&mut this.io, cx, &mut buf, &mut off) {
                        Poll::Ready(Ok(true)) => (),
                        Poll::Ready(Ok(false)) => {
                            trace!("write: eof");
                            this.write_state = WriteState::Eof;
                            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                        }
                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(e))
                        }
                        Poll::Pending => {
                            this.write_state = WriteState::WriteLen{ len, buf, off };

                            return Poll::Pending
                        }
                    }
                    this.write_state = WriteState::WriteData { len, off: 0 }
                }
                WriteState::WriteData { len, ref mut off } => {
                    let n = match ready!(
                        Pin::new(&mut this.io).poll_write(cx, &buffer.write_crypto[*off .. len])
                    ) {
                        Ok(n) => n,
                        Err(e) => return Poll::Ready(Err(e)),
                    };
                    trace!("write: wrote {}/{} bytes", *off + n, len);
                    if n == 0 {
                        trace!("write: eof");
                        this.write_state = WriteState::Eof;
                        return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                    }
                    *off += n;
                    if len == *off {
                        trace!("write: finished writing {} bytes", len);
                        this.write_state = WriteState::Init
                    }
                }
                WriteState::Eof => {
                    trace!("write: eof");
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                }
                WriteState::EncErr => return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
            }
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>
    ) -> Poll<Result<(), std::io::Error>> {
        let mut this = self.deref_mut();

        let buffer = this.buffer.borrow_mut();

        loop {
            match this.write_state {
                WriteState::Init => return Pin::new(&mut this.io).poll_flush(cx),
                WriteState::BufferData { off } => {
                    trace!("flush: encrypting {} bytes", off);
                    match this.session.write_message(&buffer.write[.. off], buffer.write_crypto) {
                        Ok(n) => {
                            trace!("flush: cipher text len = {} bytes", n);
                            this.write_state = WriteState::WriteLen {
                                len: n,
                                buf: u16::to_be_bytes(n as u16),
                                off: 0
                            }
                        }
                        Err(e) => {
                            debug!("encryption error: {:?}", e);
                            this.write_state = WriteState::EncErr;
                            return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
                        }
                    }
                }
                WriteState::WriteLen { len, mut buf, mut off } => {
                    trace!("flush: writing len ({}, {:?}, {}/2)", len, buf, off);
                    match write_frame_len(&mut this.io, cx, &mut buf, &mut off) {
                        Poll::Ready(Ok(true)) => (),
                        Poll::Ready(Ok(false)) => {
                            trace!("write: eof");
                            this.write_state = WriteState::Eof;
                            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                        }
                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(e))
                        }
                        Poll::Pending => {
                            this.write_state = WriteState::WriteLen { len, buf, off };

                            return Poll::Pending
                        }
                    }
                    this.write_state = WriteState::WriteData { len, off: 0 }
                }
                WriteState::WriteData { len, ref mut off } => {
                    let n = match ready!(
                        Pin::new(&mut this.io).poll_write(cx, &buffer.write_crypto[*off .. len])
                    ) {
                        Ok(n) => n,
                        Err(e) => return Poll::Ready(Err(e)),
                    };
                    trace!("flush: wrote {}/{} bytes", *off + n, len);
                    if n == 0 {
                        trace!("flush: eof");
                        this.write_state = WriteState::Eof;
                        return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                    }
                    *off += n;
                    if len == *off {
                        trace!("flush: finished writing {} bytes", len);
                        this.write_state = WriteState::Init;
                    }
                }
                WriteState::Eof => {
                    trace!("flush: eof");
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()))
                }
                WriteState::EncErr => return Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
            }
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>>{
        ready!(self.as_mut().poll_flush(cx))?;
        Pin::new(&mut self.io).poll_close(cx)
    }
}

/// Read 2 bytes as frame length from the given source into the given buffer.
///
/// Panics if `off >= 2`.
///
/// When [`io::ErrorKind::WouldBlock`] is returned, the given buffer and offset
/// may have been updated (i.e. a byte may have been read) and must be preserved
/// for the next invocation.
///
/// Returns `None` if EOF has been encountered.
fn read_frame_len<R: AsyncRead + Unpin>(
    mut io: &mut R,
    cx: &mut Context<'_>,
    buf: &mut [u8; 2],
    off: &mut usize,
) -> Poll<Result<Option<u16>, std::io::Error>> {
    loop {
        match ready!(Pin::new(&mut io).poll_read(cx, &mut buf[*off ..])) {
            Ok(n) => {
                if n == 0 {
                    return Poll::Ready(Ok(None));
                }
                *off += n;
                if *off == 2 {
                    return Poll::Ready(Ok(Some(u16::from_be_bytes(*buf))));
                }
            },
            Err(e) => {
                return Poll::Ready(Err(e));
            },
        }
    }
}

/// Write 2 bytes as frame length from the given buffer into the given sink.
///
/// Panics if `off >= 2`.
///
/// When [`io::ErrorKind::WouldBlock`] is returned, the given offset
/// may have been updated (i.e. a byte may have been written) and must
/// be preserved for the next invocation.
///
/// Returns `false` if EOF has been encountered.
fn write_frame_len<W: AsyncWrite + Unpin>(
    mut io: &mut W,
    cx: &mut Context<'_>,
    buf: &[u8; 2],
    off: &mut usize,
) -> Poll<Result<bool, std::io::Error>> {
    loop {
        match ready!(Pin::new(&mut io).poll_write(cx, &buf[*off ..])) {
            Ok(n) => {
                if n == 0 {
                    return Poll::Ready(Ok(false))
                }
                *off += n;
                if *off == 2 {
                    return Poll::Ready(Ok(true))
                }
            }
            Err(e) => {
                return Poll::Ready(Err(e));
            }
        }
    }
}
