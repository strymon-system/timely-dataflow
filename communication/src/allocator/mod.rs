//! Types and traits for the allocation of channels.

use std::rc::Rc;
use std::cell::RefCell;
use std::time::Duration;
use std::collections::VecDeque;
use std::sync::mpsc::{Sender, Receiver};

pub use self::thread::Thread;
pub use self::process::Process;
pub use self::generic::{Generic, GenericBuilder};

pub mod thread;
pub mod process;
pub mod generic;

pub mod canary;
pub mod counters;

pub mod zero_copy;

use crate::{Data, Push, Pull, Message};
use crate::allocator::zero_copy::bytes_exchange::MergeQueue;
use crate::allocator::zero_copy::push_pull::Pusher;

/// A proto-allocator, which implements `Send` and can be completed with `build`.
///
/// This trait exists because some allocators contain elements that do not implement
/// the `Send` trait, for example `Rc` wrappers for shared state. As such, what we
/// actually need to create to initialize a computation are builders, which we can
/// then move into new threads each of which then construct their actual allocator.
pub trait AllocateBuilder : Send {
    /// The type of allocator to be built.
    type Allocator: Allocate;
    /// Builds allocator, consumes self.
    fn build(self) -> Self::Allocator;
}

// TODO(lorenzo) doc

/// Alias with Push trait
pub trait OnNewPushFn<T>: FnMut(Box<Push<Message<T>>>) + 'static {}
impl<T,                F: FnMut(Box<Push<Message<T>>>) + 'static> OnNewPushFn<T> for F {}

/// A type capable of allocating channels.
///
/// There is some feature creep, in that this contains several convenience methods about the nature
/// of the allocated channels, and maintenance methods to ensure that they move records around.
pub trait Allocate {
    /// The index of the worker out of `(0..self.peers())`.
    fn index(&self) -> usize;
    /// The number of workers in the communication group.
    fn peers(&self) -> usize;
    /// Constructs several send endpoints and one receive endpoint.
    fn allocate<T: Data, F>(&mut self, identifier: usize, on_new_pusher: F) -> Box<Pull<Message<T>>>
         where F: OnNewPushFn<T>;

    /// If the allocator supports rescaling (atm only TcpAllocator does) and a worker
    /// joined the cluster, then back-fill all existing allocation with the new pushers
    fn rescale(&mut self) { /* nop by default */ }

    /// A shared queue of communication events with channel identifier.
    ///
    /// It is expected that users of the channel allocator will regularly
    /// drain these events in order to drive their computation. If they
    /// fail to do so the event queue may become quite large, and turn
    /// into a performance problem.
    fn events(&self) -> &Rc<RefCell<VecDeque<(usize, Event)>>>;

    /// Awaits communication events.
    ///
    /// This method may park the current thread, for at most `duration`,
    /// until new events arrive.
    /// The method is not guaranteed to wait for any amount of time, but
    /// good implementations should use this as a hint to park the thread.
    fn await_events(&self, _duration: Option<Duration>) { }

    /// Ensure that received messages are surfaced in each channel.
    ///
    /// This method should be called to ensure that received messages are
    /// surfaced in each channel, but failing to call the method does not
    /// ensure that they are not surfaced.
    ///
    /// Generally, this method is the indication that the allocator should
    /// present messages contained in otherwise scarce resources (for example
    /// network buffers), under the premise that someone is about to consume
    /// the messages and release the resources.
    fn receive(&mut self) { }

    /// Signal the completion of a batch of reads from channels.
    ///
    /// Conventionally, this method signals to the communication fabric
    /// that the worker is taking a break from reading from channels, and
    /// the fabric should consider re-acquiring scarce resources. This can
    /// lead to the fabric performing defensive copies out of un-consumed
    /// buffers, and can be a performance problem if invoked casually.
    fn release(&mut self) { }

    /// Constructs a pipeline channel from the worker to itself.
    ///
    /// By default, this method uses the thread-local channel constructor
    /// based on a shared `VecDeque` which updates the event queue.
    fn pipeline<T: 'static>(&mut self, identifier: usize) ->
        (thread::ThreadPusher<Message<T>>,
         thread::ThreadPuller<Message<T>>)
    {
        thread::Thread::new_from(identifier, self.events().clone())
    }
}

/// A communication channel event.
pub enum Event {
    /// A number of messages pushed into the channel.
    Pushed(usize),
    /// A number of messages pulled from the channel.
    Pulled(usize),
}
