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

//! A library providing high-level abstractions for event loop concurrency,
//! heavily borrowing ideas from [KJ](https://capnproto.org/cxxrpc.html#kj-concurrency-framework).
//! Allows for coordination of asynchronous tasks using [promises](struct.Promise.html) as
//! a basic building block.

extern crate mio;
extern crate nix;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use private::{promise_node, Event, BoolEvent, PromiseAndFulfillerHub,
              EVENT_LOOP, with_current_event_loop, PromiseNode};

pub mod io;

mod private;
mod handle_table;

pub type Error = Box<::std::error::Error>;
pub type Result<T> = ::std::result::Result<T, Error>;


/// A computation that might eventually resolve to a value of type `T`.
pub struct Promise<T> where T: 'static {
    node : Box<PromiseNode<T>>,
}

impl <T> Promise <T> {
    /// Chains further computation to be executed once the promise resolves.
    /// When the promise is fulfilled successfully, invokes `func` on its result.
    /// When the promise is rejected, invokes `error_handler` on the resulting error.
    ///
    /// If the returned promise is dropped before the chained computation runs, the chained
    /// computation will be cancelled.
    ///
    /// Always returns immediately, even if the promise is already resolved. The earliest that
    /// `func` or `error_handler` might be invoked is during the next `turn()` of the event loop.
    pub fn then_else<F, G, R>(self, func: F, error_handler: G) -> Promise<R>
        where F: 'static,
              F: FnOnce(T) -> Result<Promise<R>>,
              G: 'static,
              G: FnOnce(Error) -> Result<Promise<R>>,
              R: 'static
    {
        let intermediate = Box::new(promise_node::Transform::new(self.node, func, error_handler));
        Promise { node: Box::new(promise_node::Chain::new(intermediate)) }
    }

    /// Calls `then_else()` with a default error handler that simply propagates all errors.
    pub fn then<F, R>(self, func: F) -> Promise<R>
        where F: 'static,
              F: FnOnce(T) -> Result<Promise<R>>,
              R: 'static
    {
        self.then_else(func, |e| { return Err(e); })
    }

    /// Like then_else() but for a `func` that returns a direct value rather than a promise.
    pub fn map_else<F, G, R>(self, func: F, error_handler: G) -> Promise<R>
        where F: 'static,
              F: FnOnce(T) -> Result<R>,
              G: 'static,
              G: FnOnce(Error) -> Result<R>,
              R: 'static
    {
        Promise { node: Box::new(promise_node::Transform::new(self.node, func, error_handler)) }
    }

    /// Calls `map_else()` with a default error handler that simple propagates all errors.
    pub fn map<F, R>(self, func: F) -> Promise<R>
        where F: 'static,
              F: FnOnce(T) -> Result<R>,
              R: 'static
    {
        self.map_else(func, |e| { return Err(e); })
    }

    /// Returns a new promise that resolves when either `self` or `other` resolves. The promise that
    /// doesn't resolve first is cancelled.
    pub fn exclusive_join(self, other: Promise<T>) -> Promise<T> {
        return Promise { node: Box::new(private::promise_node::ExclusiveJoin::new(self.node, other.node)) };
    }

    /// Creates a new promise that has already been fulfilled.
    pub fn fulfilled(value: T) -> Promise<T> {
        return Promise { node: Box::new(promise_node::Immediate::new(Ok(value))) };
    }

    /// Creates a new promise that has already been rejected with the given error.
    pub fn rejected(error: Error) -> Promise<T> {
        return Promise { node: Box::new(promise_node::Immediate::new(Err(error))) };
    }

    /// Runs the event loop until the promise is fulfilled.
    ///
    /// The `WaitScope` argument ensures that `wait()` can only be called at the top level of a program.
    /// Waiting within event callbacks is disallowed.
    pub fn wait(mut self, _wait_scope: &WaitScope) -> Result<T> {
        with_current_event_loop(move |event_loop| {
            let fired = ::std::rc::Rc::new(::std::cell::Cell::new(false));
            let done_event = BoolEvent::new(fired.clone());
            let (handle, _dropper) = private::EventHandle::new();
            handle.set(Box::new(done_event));
            self.node.on_ready(handle);

            //event_loop.running = true;

            while !fired.get() {
                if !event_loop.turn() {
                    // No events in the queue.
                    event_loop.event_port.borrow_mut().wait();
                }
            }

            self.node.get()
        })
    }
}

/// A scope in which asynchronous programming can occur. Corresponds to the top level scope
/// of some event loop.
pub struct WaitScope(::std::marker::PhantomData<*mut u8>); // impl !Sync for WaitScope {}

/// Interface between an `EventLoop` and events originating from outside of the loop's thread.
trait EventPort {
    /// Waits for an external event to arrive, sleeping if necessary.
    /// Returns true if wake() has been called from another thread.
    fn wait(&mut self) -> bool;

    /// Checks whether any external events have arrived, but does not sleep.
    /// Returns true if wake() has been called from another thread.
    fn poll(&mut self) -> bool;

    /// Called to notify the `EventPort` when the `EventLoop` has work to do; specifically when it
    /// transitions from empty -> runnable or runnable -> empty. This is typically useful when
    /// integrating with an external event loop; if the loop is currently runnable then you should
    /// arrange to call run() on it soon. The default implementation does nothing.
    fn set_runnable(&mut self, _runnable: bool) { }


    fn wake(&mut self) { unimplemented!(); }
}

/// A queue of events being executed in a loop on a single thread.
pub struct EventLoop {
//    daemons: TaskSetImpl,
    event_port: RefCell<io::MioEventPort>,
    _running: bool,
    _last_runnable_state: bool,
    events: RefCell<handle_table::HandleTable<private::EventNode>>,
    head: private::EventHandle,
    tail: Cell<private::EventHandle>,
    depth_first_insertion_point: Cell<private::EventHandle>,
}



impl EventLoop {
    /// Creates an event loop for the current thread, panicking if one already exists. Runs the given
    /// closure and then drops the event loop.
    pub fn top_level<F>(main: F) -> Result<()>
        where F: FnOnce(&WaitScope) -> Result<()>
    {
        let mut events = handle_table::HandleTable::<private::EventNode>::new();
        let dummy = private::EventNode { event: None, next: None, prev: None };
        let head_handle = private::EventHandle(events.push(dummy));

        EVENT_LOOP.with(move |maybe_event_loop| {
            let event_loop = EventLoop {
                event_port: RefCell::new(io::MioEventPort::new().unwrap()),
                _running: false,
                _last_runnable_state: false,
                events: RefCell::new(events),
                head: head_handle,
                tail: Cell::new(head_handle),
                depth_first_insertion_point: Cell::new(head_handle), // insert after this node
            };

            assert!(maybe_event_loop.borrow().is_none());
            *maybe_event_loop.borrow_mut() = Some(event_loop);
        });
        let wait_scope = WaitScope(::std::marker::PhantomData );

        let result = main(&wait_scope);

        EVENT_LOOP.with(move |maybe_event_loop| {
            *maybe_event_loop.borrow_mut() = None;
        });

        return result;
    }

    fn arm_depth_first(&self, event_handle: private::EventHandle) {

        let insertion_node_next = self.events.borrow()[self.depth_first_insertion_point.get().0].next;

        match insertion_node_next {
            Some(next_handle) => {
                self.events.borrow_mut()[next_handle.0].prev = Some(event_handle);
                self.events.borrow_mut()[event_handle.0].next = Some(next_handle);
            }
            None => {
                self.tail.set(event_handle);
            }
        }

        self.events.borrow_mut()[event_handle.0].prev = Some(self.depth_first_insertion_point.get());
        self.events.borrow_mut()[self.depth_first_insertion_point.get().0].next = Some(event_handle);
        self.depth_first_insertion_point.set(event_handle);
    }

    fn arm_breadth_first(&self, event_handle: private::EventHandle) {
        let events = &mut *self.events.borrow_mut();
        events[self.tail.get().0].next = Some(event_handle);
        events[event_handle.0].prev = Some(self.tail.get());
        self.tail.set(event_handle);
    }

    /// Runs the event loop for `max_turn_count` turns or until there is nothing left to be done,
    /// whichever comes first. This never calls the `EventPort`'s `sleep()` or `poll()`. It will
    /// call the `EventPort`'s `set_runnable(false)` if the queue becomes empty.
    fn _run(&mut self, max_turn_count: u32) {
        self._running = true;

        for _ in 0..max_turn_count {
            if !self.turn() {
                break;
            }
        }
    }

    /// Runs the event loop for a single step.
    fn turn(&self) -> bool {

        let event_handle = match self.events.borrow()[self.head.0].next {
            None => return false,
            Some(event_handle) => { event_handle }
        };
        self.depth_first_insertion_point.set(event_handle);

        let mut event = ::std::mem::replace(&mut self.events.borrow_mut()[event_handle.0].event, None)
            .expect("No event to fire?");
        let _dropper = event.fire();

        let maybe_next = self.events.borrow()[event_handle.0].next;
        self.events.borrow_mut()[self.head.0].next = maybe_next;
        match maybe_next {
            Some(e) => {
                self.events.borrow_mut()[e.0].prev = Some(self.head);
            }
            None => {}
        }

        self.events.borrow_mut()[event_handle.0].next = None;
        self.events.borrow_mut()[event_handle.0].prev = None;

        if self.tail.get() == event_handle {
            self.tail.set(self.head);
        }

        self.depth_first_insertion_point.set(self.head);
        return true;
    }
}

/// A callback that can be used to fulfill or reject a promise.
pub trait PromiseFulfiller<T> where T: 'static {
    fn fulfill(self: Box<Self>, value: T);
    fn reject(self: Box<Self>, error: Error);
}

/// Creates a new promise/fulfiller pair.
pub fn new_promise_and_fulfiller<T>() -> (Promise<T>, Box<PromiseFulfiller<T>>) where T: 'static {
    let result = ::std::rc::Rc::new(::std::cell::RefCell::new(PromiseAndFulfillerHub::new()));
    let result_promise : Promise<T> = Promise { node: Box::new(result.clone())};
    (result_promise, Box::new(result))
}


/// Holds a collection of `Promise<()>`s and ensures that each executes to completion.
/// Destroying a TaskSet automatically cancels all of its unfinished promises.
pub struct TaskSet {
    task_set_impl: Rc<RefCell<private::TaskSetImpl>>,
}

impl TaskSet {
    pub fn new(error_handler: Box<ErrorHandler>) -> TaskSet {
        TaskSet { task_set_impl : Rc::new(RefCell::new(private::TaskSetImpl::new(error_handler))) }
    }

    pub fn add(&mut self, promise: Promise<()>) {
        private::TaskSetImpl::add(self.task_set_impl.clone(), promise.node);
    }
}

/// A callback to be invoked when a task in a `TaskSet` fails.
pub trait ErrorHandler {
    fn task_failed(&mut self, error: Error);
}

/// Transforms a vector of promises into a promise for a vector.
pub fn join_promises<T>(promises: Vec<Promise<T>>) -> Promise<Vec<T>> {
    let nodes = promises.into_iter().map(|p| { p.node }).collect();
    Promise { node: Box::new(private::promise_node::ArrayJoin::new(nodes)) }
}
