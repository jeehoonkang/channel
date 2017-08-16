// Based on Dmitry Vyukov's MPSC queue:
// http://www.1024cores.net/home/lock-free-algorithms/queues/non-intrusive-mpsc-node-based-queue

use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, SeqCst};
use std::thread;
use std::time::{Duration, Instant};

use epoch::{self, Atomic, Owned};

use CaseId;
use actor;
use err::{RecvError, RecvTimeoutError, SendError, SendTimeoutError, TryRecvError, TrySendError};
use monitor::Monitor;

struct Node<T> {
    next: Atomic<Node<T>>,
    value: T,
}

#[repr(C)]
pub(crate) struct Channel<T> {
    head: Atomic<Node<T>>,
    recv_count: AtomicUsize,
    _pad0: [u8; 64],
    tail: Atomic<Node<T>>,
    send_count: AtomicUsize,
    _pad1: [u8; 64],
    closed: AtomicBool,
    receivers: Monitor,
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    pub fn new() -> Self {
        // Initialize the internal representation of the queue.
        let queue = Channel {
            head: Atomic::null(),
            tail: Atomic::null(),
            closed: AtomicBool::new(false),
            receivers: Monitor::new(),
            send_count: AtomicUsize::new(0),
            recv_count: AtomicUsize::new(0),
            _pad0: [0; 64],
            _pad1: [0; 64],
            _marker: PhantomData,
        };

        // Create a none node.
        let node = Owned::new(Node {
            value: unsafe { mem::uninitialized() },
            next: Atomic::null(),
        });

        unsafe {
            epoch::unprotected(|scope| {
                let node = node.into_ptr(scope);
                queue.head.store(node, Relaxed);
                queue.tail.store(node, Relaxed);
            })
        }

        queue
    }

    fn push(&self, value: T) {
        let mut node = Owned::new(Node {
            value: value,
            next: Atomic::null(),
        });

        unsafe {
            epoch::unprotected(|scope| {
                let new = node.into_ptr(scope);
                let old = self.tail.swap(new, SeqCst, scope);
                self.send_count.fetch_add(1, SeqCst);
                old.deref().next.store(new, SeqCst);
            })
        }
    }

    fn pop(&self) -> Option<T> {
        const USE: usize = 1;
        const MULTI: usize = 2;

        // TODO: finer grained unsafe code
        return unsafe {
            epoch::unprotected(|scope| {
                if self.head.load(Relaxed, scope).tag() & MULTI == 0 {
                    loop {
                        let head = self.head.fetch_or(USE, SeqCst, scope);
                        if head.tag() != 0 {
                            break;
                        }

                        let next = head.deref().next.load(SeqCst, scope);

                        if next.is_null() {
                            self.head.fetch_and(!USE, SeqCst, scope);

                            if self.tail.load(SeqCst, scope).as_raw() == head.as_raw() {
                                return None;
                            }
                        } else {
                            let value = ptr::read(&next.deref().value);

                            if self.head
                                .compare_and_set(head.with_tag(USE), next, SeqCst, scope)
                                .is_ok()
                            {
                                self.recv_count.fetch_add(1, SeqCst);
                                Vec::from_raw_parts(head.as_raw() as *mut Node<T>, 0, 1);
                                return Some(value);
                            }
                            mem::forget(value);

                            self.head.fetch_and(!USE, SeqCst, scope);
                        }
                    }

                    self.head.fetch_or(MULTI, SeqCst, scope);
                    while self.head.load(SeqCst, scope).tag() & USE != 0 {
                        thread::yield_now();
                    }
                }

                epoch::pin(|scope| loop {
                    let head = self.head.load(SeqCst, scope);
                    let next = head.deref().next.load(SeqCst, scope);

                    if next.is_null() {
                        if self.tail.load(SeqCst, scope).as_raw() == head.as_raw() {
                            return None;
                        }
                    } else {
                        if self.head
                            .compare_and_set(head, next.with_tag(MULTI), SeqCst, scope)
                            .is_ok()
                        {
                            self.recv_count.fetch_add(1, SeqCst);
                            scope.defer_free(head);
                            return Some(ptr::read(&next.deref().value));
                        }
                    }
                })
            })
        };
    }

    pub fn len(&self) -> usize {
        loop {
            let send_count = self.send_count.load(SeqCst);
            let recv_count = self.recv_count.load(SeqCst);

            if self.send_count.load(SeqCst) == send_count {
                return send_count.wrapping_sub(recv_count);
            }
        }
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        if self.closed.load(SeqCst) {
            Err(TrySendError::Disconnected(value))
        } else {
            self.push(value);
            self.receivers.notify_one();
            Ok(())
        }
    }

    pub fn send(&self, value: T) -> Result<(), SendTimeoutError<T>> {
        if self.closed.load(SeqCst) {
            Err(SendTimeoutError::Disconnected(value))
        } else {
            self.push(value);
            self.receivers.notify_one();
            Ok(())
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let closed = self.closed.load(SeqCst);
        match self.pop() {
            None => if closed {
                Err(TryRecvError::Disconnected)
            } else {
                Err(TryRecvError::Empty)
            },
            Some(v) => Ok(v),
        }
    }

    pub fn spin_try_recv(&self) -> Result<T, TryRecvError> {
        for i in 0..20 {
            let closed = self.closed.load(SeqCst);
            if let Some(v) = self.pop() {
                return Ok(v);
            }
            if closed {
                return Err(TryRecvError::Disconnected);
            }
        }
        Err(TryRecvError::Empty)
    }

    pub fn recv_until(
        &self,
        deadline: Option<Instant>,
        case_id: CaseId,
    ) -> Result<T, RecvTimeoutError> {
        loop {
            match self.spin_try_recv() {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
            }

            actor::current_reset();
            self.receivers.register(case_id);
            let timed_out =
                !self.is_closed() && self.len() == 0 && !actor::current_wait_until(deadline);
            self.receivers.unregister(case_id);

            if timed_out {
                return Err(RecvTimeoutError::Timeout);
            }
        }
    }

    pub fn close(&self) -> bool {
        if self.closed.swap(true, SeqCst) {
            false
        } else {
            self.receivers.abort_all();
            true
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(SeqCst)
    }

    pub fn receivers(&self) -> &Monitor {
        &self.receivers
    }
}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        unsafe {
            epoch::unprotected(|scope| {
                let mut head = self.head.load(Relaxed, scope);
                while !head.is_null() {
                    let next = head.deref().next.load(Relaxed, scope);

                    if let Some(n) = next.as_ref() {
                        ptr::drop_in_place(&n.value as *const _ as *mut Node<T>)
                    }

                    Vec::from_raw_parts(head.as_raw() as *mut Node<T>, 0, 1);
                    head = next;
                }
            });
        }
    }
}
