// Copyright (c) 2013-2015 Sandstorm Development Group, Inc. and contributors
// Licensed under the MIT License:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

//! Asynchronous input and output.

use std::ops::{DerefMut, Deref};
use handle_table::{HandleTable, Handle};
use {EventLoop, EventPort, Promise, PromiseFulfiller, Result, WaitScope, new_promise_and_fulfiller};
use private::{with_current_event_loop};


/// A nonblocking input bytestream.
pub trait AsyncRead: 'static {

    /// Attempts to read `buf.len()` bytes from the stream, writing them into `buf`.
    /// Returns `self`, the modified `buf`, and the number of bytes actually read.
    /// Returns as soon as `min_bytes` are read or EOF is encountered.
    fn try_read<T>(self, buf: T, min_bytes: usize) -> Promise<(Self, T, usize)>
        where T: DerefMut<Target=[u8]>;

    /// Like `try_read()`, but returns an error if EOF is encountered before `min_bytes`
    /// can be read.
    fn read<T>(self, buf: T, min_bytes: usize) -> Promise<(Self, T, usize)>
        where T: DerefMut<Target=[u8]>, Self: Sized
    {
        return self.try_read(buf, min_bytes).map(move |(s, buf, n)| {
            if n < min_bytes {
                return Err(Box::new(::std::io::Error::new(::std::io::ErrorKind::Other, "Premature EOF")))
            } else {
                return Ok((s, buf, n));
            }
        });
    }
}

/// A nonblocking output bytestream.
pub trait AsyncWrite: 'static {
    /// Attempts to write all `buf.len()` bytes from `buf` into the stream. Returns `self` and `buf`
    /// once all of the bytes have been written.
    fn write<T>(self, buf: T) -> Promise<(Self, T)> where T: Deref<Target=[u8]>;
}

pub struct Slice<T> where T: Deref<Target=[u8]> {
    pub buf: T,
    pub end: usize,
}

impl <T> Slice<T> where T: Deref<Target=[u8]> {
    pub fn new(buf: T, end: usize) -> Slice<T> {
        Slice { buf: buf, end: end }
    }
}

impl <T> Deref for Slice<T> where T: Deref<Target=[u8]> {
    type Target=[u8];
    fn deref<'a>(&'a self) -> &'a [u8] {
        &self.buf[0..self.end]
    }
}

fn register_new_handle<E>(evented: &E) -> Result<Handle> where E: ::mio::Evented {
    let handle = FdObserver::new();
    let token = ::mio::Token(handle.val);
    return with_current_event_loop(move |event_loop| {
        try!(event_loop.event_port.borrow_mut().reactor.register_opt(evented, token,
                                                                     ::mio::Interest::writable() |
                                                                     ::mio::Interest::readable(),
                                                                     ::mio::PollOpt::edge()));
        // XXX if this fails, the handle does not get cleanedup.

        return Ok(handle);
    });
}

#[derive(Copy, Clone)]
pub struct NetworkAddress {
    address: ::std::net::SocketAddr,
}

impl NetworkAddress {
    pub fn new<T : ::std::net::ToSocketAddrs>(address: T) -> Result<NetworkAddress> {
        match try!(address.to_socket_addrs()).next() {
            Some(addr) => return Ok(NetworkAddress { address: addr }),
            None => unimplemented!(),
        }
    }

    pub fn listen(self) -> Result<ConnectionReceiver> {
        let socket = try!(::mio::tcp::TcpSocket::v4());
        try!(socket.set_reuseaddr(true));
        try!(socket.bind(&self.address));
        let handle = FdObserver::new();
        let listener = try!(socket.listen(256));

        return with_current_event_loop(move |event_loop| {
            try!(event_loop.event_port.borrow_mut().reactor.register_opt(&listener, ::mio::Token(handle.val),
                                                                         ::mio::Interest::readable(),
                                                                         ::mio::PollOpt::edge()));
            Ok(ConnectionReceiver { listener: listener,
                                    handle: handle })
        });
    }

    pub fn connect(self) -> Promise<TcpStream> {
        return Promise::fulfilled(()).then(move |()| {
            let socket = try!(::mio::tcp::TcpSocket::v4());
            let (stream, connected) = try!(socket.connect(&self.address));

            // TODO: if we're not already connected, maybe only register writable interest,
            // and then reregister with read/write interested once we successfully connect.

            let handle = try!(register_new_handle(&stream));

            if connected {
                return Ok(Promise::fulfilled(TcpStream::new(stream, handle)));
            } else {
                return with_current_event_loop(move |event_loop| {
                    let promise =
                        event_loop.event_port.borrow_mut().handler.observers[handle].when_becomes_writable();

                    return Ok(promise.map(move |()| {
                        // TODO check for error.
                        return Ok(TcpStream::new(stream, handle));
                    }));
                });
            }
        });
    }
}


pub struct ConnectionReceiver {
    listener: ::mio::tcp::TcpListener,
    handle: Handle,
}

impl Drop for ConnectionReceiver {
    fn drop(&mut self) {
        // deregister the token
    }
}

impl ConnectionReceiver {

    fn accept_internal(self) -> Result<Promise<(ConnectionReceiver, TcpStream)>> {
        let accept_result = try!(self.listener.accept());
        match accept_result {
            Some(stream) => {
                let handle = try!(register_new_handle(&stream));
                return Ok(Promise::fulfilled((self, TcpStream::new(stream, handle))));
            }
            None => {
                return with_current_event_loop(move |event_loop| {
                    let promise =
                        event_loop.event_port.borrow_mut().handler.observers[self.handle].when_becomes_readable();
                    return Ok(promise.then(move |()| {
                        return self.accept_internal();
                    }));
                });
            }
        }
    }


    pub fn accept(self) -> Promise<(ConnectionReceiver, TcpStream)> {
        return Promise::fulfilled(()).then(move |()| {return self.accept_internal(); });
    }
}

pub struct TcpStream {
    stream: ::mio::tcp::TcpStream,
    handle: Handle,
}

impl ::mio::TryRead for TcpStream {
    fn try_read(&mut self, buf: &mut [u8]) -> ::std::io::Result<Option<usize>> {
        use mio::TryRead;
        self.stream.try_read(buf)
    }
}

impl ::mio::TryWrite for TcpStream {
    fn try_write(&mut self, buf: &[u8]) -> ::std::io::Result<Option<usize>> {
        use mio::TryWrite;
        self.stream.try_write(buf)
    }
}

trait HasHandle {
    fn get_handle(&self) -> Handle;
}

impl HasHandle for TcpStream {
    fn get_handle(&self) -> Handle { self.handle }
}


impl Drop for TcpStream {
    fn drop(&mut self) {
        return with_current_event_loop(move |event_loop| {
            event_loop.event_port.borrow_mut().handler.observers.remove(self.handle);
            let _ = event_loop.event_port.borrow_mut().reactor.deregister(&self.stream);
        });
    }
}

impl TcpStream {
    fn new(stream: ::mio::tcp::TcpStream, handle: Handle) -> TcpStream {
        TcpStream { stream: stream, handle: handle }
    }

    pub fn try_clone(&self) -> Result<TcpStream> {
        let stream = try!(self.stream.try_clone());
        let handle = try!(register_new_handle(&stream));
        return Ok(TcpStream::new(stream, handle));
    }
}


fn try_read_internal<R, T>(mut reader: R,
                           mut buf: T,
                           mut already_read: usize,
                           min_bytes: usize) -> Result<Promise<(R, T, usize)>>
    where T: DerefMut<Target=[u8]>, R: ::mio::TryRead + HasHandle
{
    use mio::TryRead;

    while already_read < min_bytes {
        let read_result = try!(reader.try_read(&mut buf[already_read..]));
        match read_result {
            Some(0) => {
                // EOF
                return Ok(Promise::fulfilled((reader, buf, already_read)));
            }
            Some(n) => {
                already_read += n;
            }
            None => { // would block
                return with_current_event_loop(move |event_loop| {
                    let promise =
                        event_loop.event_port.borrow_mut()
                        .handler.observers[reader.get_handle()].when_becomes_readable();
                    return Ok(promise.then(move |()| {
                        return try_read_internal(reader, buf, already_read, min_bytes);
                    }));
                });
            }
        }
    }

    return Ok(Promise::fulfilled((reader, buf, already_read)));
}

fn write_internal<W, T>(mut writer: W,
                        buf: T,
                        mut already_written: usize) -> Result<Promise<(W, T)>>
    where T: Deref<Target=[u8]>, W: ::mio::TryWrite + HasHandle
{
    use mio::TryWrite;

    while already_written < buf.len() {
        let write_result = try!(writer.try_write(&buf[already_written..]));
        match write_result {
            Some(n) => {
                already_written += n;
            }
            None => { // would block
                return with_current_event_loop(move |event_loop| {
                    let promise =
                        event_loop.event_port.borrow_mut()
                        .handler.observers[writer.get_handle()].when_becomes_writable();
                    return Ok(promise.then(move |()| {
                        return write_internal(writer, buf, already_written);
                    }));
                });
            }
        }
    }

    return Ok(Promise::fulfilled((writer, buf)));
}


impl AsyncRead for TcpStream {
    fn try_read<T>(self, buf: T,
               min_bytes: usize) -> Promise<(Self, T, usize)> where T: DerefMut<Target=[u8]> {
        return Promise::fulfilled(()).then(move |()| {
            return try_read_internal(self, buf, 0, min_bytes);
        });
    }
}

impl AsyncWrite for TcpStream {
    fn write<T>(self, buf: T) -> Promise<(Self, T)> where T: Deref<Target=[u8]> {
        return Promise::fulfilled(()).then(move |()| {
            return write_internal(self, buf, 0);
        });
    }
}


struct FdObserver {
    read_fulfiller: Option<Box<PromiseFulfiller<()>>>,
    write_fulfiller: Option<Box<PromiseFulfiller<()>>>,
}

impl FdObserver {
    pub fn new() -> Handle {
        with_current_event_loop(move |event_loop| {

            let observer = FdObserver { read_fulfiller: None, write_fulfiller: None };
            let event_port = &mut *event_loop.event_port.borrow_mut();
            return event_port.handler.observers.push(observer);
        })
    }

    pub fn when_becomes_readable(&mut self) -> Promise<()> {
        let (promise, fulfiller) = new_promise_and_fulfiller();
        self.read_fulfiller = Some(fulfiller);
        return promise;
    }

    pub fn when_becomes_writable(&mut self) -> Promise<()> {
        let (promise, fulfiller) = new_promise_and_fulfiller();
        self.write_fulfiller = Some(fulfiller);
        return promise;
    }
}

pub struct MioEventPort {
    handler: Handler,
    reactor: ::mio::EventLoop<Handler>,
}

struct Handler {
    observers: HandleTable<FdObserver>,
}

impl MioEventPort {
    pub fn new() -> Result<MioEventPort> {
        Ok(MioEventPort {
            handler: Handler { observers: HandleTable::new() },
            reactor: try!(::mio::EventLoop::new()),
        })
    }
}

impl ::mio::Handler for Handler {
    type Timeout = Timeout;
    type Message = ();
    fn readable(&mut self, _event_loop: &mut ::mio::EventLoop<Handler>,
                token: ::mio::Token, _hint: ::mio::ReadHint) {
        match ::std::mem::replace(&mut self.observers[Handle {val: token.0}].read_fulfiller, None) {
            Some(fulfiller) => {
                fulfiller.fulfill(())
            }
            None => {
                ()
            }
        }
    }
    fn writable(&mut self, _event_loop: &mut ::mio::EventLoop<Handler>, token: ::mio::Token) {
        match ::std::mem::replace(&mut self.observers[Handle { val: token.0}].write_fulfiller, None) {
            Some(fulfiller) => fulfiller.fulfill(()),
            None => (),
        }
    }
    fn timeout(&mut self, _event_loop: &mut ::mio::EventLoop<Handler>, timeout: Timeout) {
        timeout.fulfiller.fulfill(());
    }
}

impl EventPort for MioEventPort {
    fn wait(&mut self) -> bool {
        self.reactor.run_once(&mut self.handler).unwrap();
        return false;
    }

    fn poll(&mut self) -> bool {
        self.reactor.run_once(&mut self.handler).unwrap();
        return false;
    }
}

pub struct Timer;

impl Timer {
    pub fn after_delay_ms(&self, delay: u64) -> Promise<()> {
        let (promise, fulfiller) = new_promise_and_fulfiller();
        let timeout = Timeout { fulfiller: fulfiller };
        return with_current_event_loop(move |event_loop| {
            let handle = event_loop.event_port.borrow_mut().reactor.timeout_ms(timeout, delay).unwrap();
            return
                Promise {
                    node: Box::new(
                        ::private::promise_node::Wrapper::new(promise.node,
                                                              TimeoutDropper { handle: handle })) };
        });
    }

    pub fn timeout_after_ms<T>(&self, delay: u64, promise: Promise<T>) -> Promise<T> {
        promise.exclusive_join(self.after_delay_ms(delay).map(|()| {
            return Err(Box::new(::std::io::Error::new(::std::io::ErrorKind::Other, "operation timed out")))
        }))
    }
}

struct TimeoutDropper {
    handle: ::mio::Timeout,
}

impl Drop for TimeoutDropper {
    fn drop(&mut self) {
        with_current_event_loop(move |event_loop| {
            event_loop.event_port.borrow_mut().reactor.clear_timeout(self.handle);
        });
    }
}

struct Timeout {
    fulfiller: Box<PromiseFulfiller<()>>,
}

pub struct SocketStream {
    stream: ::mio::Io,
    handle: Handle,
}

impl ::mio::TryRead for SocketStream {
    fn try_read(&mut self, buf: &mut [u8]) -> ::std::io::Result<Option<usize>> {
        use mio::TryRead;
        self.stream.try_read(buf)
    }
}

impl ::mio::TryWrite for SocketStream {
    fn try_write(&mut self, buf: &[u8]) -> ::std::io::Result<Option<usize>> {
        use mio::TryWrite;
        self.stream.try_write(buf)
    }
}

impl HasHandle for SocketStream {
    fn get_handle(&self) -> Handle { self.handle }
}

impl AsyncRead for SocketStream {
    fn try_read<T>(self, buf: T,
               min_bytes: usize) -> Promise<(Self, T, usize)> where T: DerefMut<Target=[u8]> {
        return Promise::fulfilled(()).then(move |()| {
            return try_read_internal(self, buf, 0, min_bytes);
        });
    }
}

impl AsyncWrite for SocketStream {
    fn write<T>(self, buf: T) -> Promise<(Self, T)> where T: Deref<Target=[u8]> {
        return Promise::fulfilled(()).then(move |()| {
            return write_internal(self, buf, 0);
        });
    }
}

/// Creates a new thread and sets up a socket pair that can be used to communicate with it.
/// Passes one of the sockets to the thread's start function and returns the other socket.
/// The new thread will already have an active event loop when `start_func` is called.
pub fn spawn<F>(start_func: F) -> Result<(::std::thread::JoinHandle<()>, SocketStream)>
    where F: FnOnce(SocketStream, &WaitScope) -> Result<()>,
          F: Send + 'static
{
    use nix::sys::socket::{socketpair, AddressFamily, SockType, SOCK_CLOEXEC, SOCK_NONBLOCK};

    let (fd0, fd1) =
        // eh... the trait `std::error::Error` is not implemented for the type `nix::Error`
        match socketpair(AddressFamily::Unix, SockType::Stream, 0, SOCK_NONBLOCK | SOCK_CLOEXEC) {
            Ok(v) => v,
            Err(_) => {
                return Err(Box::new(::std::io::Error::new(::std::io::ErrorKind::Other,
                                                          "failed to create socketpair")))
            }
        };

    let io = ::mio::Io::from_raw_fd(fd0);
    let handle = try!(register_new_handle(&io));
    let socket_stream = SocketStream { stream: io, handle: handle };

    let join_handle = ::std::thread::spawn(move || {
        let _result = EventLoop::top_level(move |wait_scope| {
            let io = ::mio::Io::from_raw_fd(fd1);
            let handle = try!(register_new_handle(&io));
            let socket_stream = SocketStream { stream: io, handle: handle };
            start_func(socket_stream, &wait_scope)
        });
    });

    return Ok((join_handle, socket_stream));
}
