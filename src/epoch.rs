//!
//! Lifetime-like runtime borrow checker.
//!

use std::cmp::min;
use std::collections::VecDeque;
use std::ops::{Add, AddAssign, Deref, Range, Sub};
use std::sync::atomic::{AtomicUsize, Ordering};

use relevant::Relevant;

/// Epoch identifier.
/// User can compare `Epoch`s with one another to check which is "earlier".
/// Primary used with `ValidUntil` implementations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Epoch(pub u64);

impl Epoch {
    /// Create new `Epoch` that never compared as "later" to any other.
    pub fn new() -> Self {
        Epoch(0)
    }
}

impl Add<u64> for Epoch {
    type Output = Epoch;
    fn add(self, add: u64) -> Epoch {
        Epoch(self.0 + add)
    }
}

impl AddAssign<u64> for Epoch {
    fn add_assign(&mut self, add: u64) {
        self.0 += add;
    }
}

impl Sub<Epoch> for Epoch {
    type Output = u64;
    fn sub(self, sub: Epoch) -> u64 {
        self.0 - sub.0
    }
}

/// Epoch counter.
/// Place it somewhere where users can access it.
/// This type shouldn't be instantiated more then once.
#[derive(Debug)]
pub struct CurrentEpoch(u64);

impl CurrentEpoch {
    /// Create new epoch counter.
    pub fn new() -> Self {
        CurrentEpoch(1)
    }

    /// Get current epoch identifier.
    pub fn now(&self) -> Epoch {
        Epoch(self.0)
    }

    /// Advance to next epoch.
    pub fn advance(&mut self, count: u64) {
        self.0 += count;
    }
}

pub trait ValidUntil {
    /// Encapsulated data.
    type Data;

    /// Get last epoch this value has to be valid until.
    fn valid_until(&self) -> Epoch;

    /// Try to dispose of this value.
    fn dispose(self, current: &CurrentEpoch) -> Result<Self::Data, Self>
    where
        Self: Sized;
}

/// Check if this value valid until specified `Epoch`
fn is_valid<T: ValidUntil>(value: &T, epoch: Epoch) -> bool {
    value.valid_until() > epoch
}

/// Weak epoch pointer to `T`.
/// It will expire after `CurrentEpoch` will advance further
/// than `Epoch` `valid_until` returns.
#[derive(Debug)]
pub struct Ec<T> {
    ptr: *const T,
    valid_until: Epoch,
}

unsafe impl<T> Send for Ec<T>
where
    T: Sync,
{
}
unsafe impl<T> Sync for Ec<T>
where
    T: Sync,
{
}

impl<T> Copy for Ec<T> {}

impl<T> Clone for Ec<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Ec<T> {
    /// Get `Epoch` after which this `Ec` will expire.
    #[inline]
    pub fn valid_until(&self) -> Epoch {
        self.valid_until
    }

    /// Get reference to the pointed value.
    /// Returns `Some` if `Ec` hasn't expired yet
    /// (`CurrentEpoch::now()` is "earlier" than `self.valid_until()`).
    /// Returns `None` otherwise.
    #[inline]
    pub fn get<'a>(&self, current: &'a CurrentEpoch) -> Option<&'a T> {
        unsafe {
            if self.valid_until > current.now() {
                Some(&*self.ptr)
            } else {
                None
            }
        }
    }

    /// Get reference to the pointed value.
    /// Returns `Some` if `Ec` won't be expired withing specified epoch range.
    /// Returns `None` otherwise.
    ///
    /// User must guarantee that `CurrentEpoch` won't be advanced to `span.end`
    /// while `T` is in use.
    #[inline]
    pub unsafe fn get_span<'a>(&self, span: Range<Epoch>) -> Option<&'a T> {
        if self.valid_until >= span.end {
            Some(&*self.ptr)
        } else {
            None
        }
    }
}

/// Strong pointer to `T`.
/// It will hold value alive and can't be disposed until `CurrentEpoch`
/// advances further than the last `Epoch` specified in `make_valid_until`
/// and `borrow` calls
#[derive(Debug)]
pub struct Eh<T> {
    relevant: Relevant,
    ptr: *const T,
    valid_until: AtomicEpoch,
}

impl<T> Eh<T> {
    /// Wrap value into `Eh`
    #[inline]
    pub fn new(value: T) -> Self {
        Eh {
            relevant: Relevant,
            ptr: Box::into_raw(Box::new(value)),
            valid_until: AtomicEpoch::new(Epoch::new()),
        }
    }

    /// Make all new `Ec` borrowed from this `Eh` to be valid
    /// until `CurrentEpoch` advances further than specified `Epoch`.
    #[inline]
    pub fn make_valid_until(this: &Self, epoch: Epoch) {
        this.valid_until.advance_to(epoch);
    }

    /// Borrow `Ec` from this `Eh`
    /// `Ec` will expire after specified `Epoch`
    #[inline]
    pub fn borrow(this: &Self, epoch: Epoch) -> Ec<T> {
        Self::make_valid_until(this, epoch);
        Ec {
            ptr: this.ptr,
            valid_until: this.valid_until.epoch(),
        }
    }
}

unsafe impl<T> Send for Eh<T>
where
    T: Send + Sync,
{
}
unsafe impl<T> Sync for Eh<T>
where
    T: Sync,
{
}

impl<T> ValidUntil for Eh<T> {
    type Data = T;

    #[inline]
    fn valid_until(&self) -> Epoch {
        self.valid_until.epoch()
    }

    #[inline]
    fn dispose(self, current: &CurrentEpoch) -> Result<T, Self> {
        if self.valid_until.epoch() <= current.now() {
            self.relevant.dispose();
            Ok(unsafe { *Box::from_raw(self.ptr as *mut _) })
        } else {
            Err(self)
        }
    }
}

impl<T> From<Box<T>> for Eh<T> {
    #[inline]
    fn from(b: Box<T>) -> Self {
        Eh {
            relevant: Relevant,
            ptr: Box::into_raw(b),
            valid_until: AtomicEpoch::new(Epoch::new()),
        }
    }
}

impl<T> Deref for Eh<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.ptr }
    }
}

/// This queue can be used to trash unused `Eh` and other implementors of `ValidUntil`.
/// It can be `clean`ed to drop all enqueued `ValidUntil` implementors that has been expired.
pub struct DeletionQueue<T> {
    offset: u64,
    queue: VecDeque<Vec<T>>,
    clean_vecs: Vec<Vec<T>>,
}

impl<T> DeletionQueue<T>
where
    T: ValidUntil,
{
    /// Create empty queue.
    #[inline]
    pub fn new() -> Self {
        DeletionQueue {
            offset: 0,
            queue: VecDeque::new(),
            clean_vecs: Vec::new(),
        }
    }

    /// Add new value to the queue.
    /// After the value expires it will be disposed
    /// with next `clean` call.
    pub fn add(&mut self, value: T) {
        let index = (value.valid_until().0 - self.offset) as usize;
        let ref mut queue = self.queue;
        let ref mut clean_vecs = self.clean_vecs;

        let len = queue.len();
        queue.extend((len..index + 1).map(|_| clean_vecs.pop().unwrap_or_else(|| Vec::new())));
        queue[index].push(value);
    }

    /// Dispose all expired enqueued values.
    /// i.e. if `value.valid_until() < current.now()`
    pub fn clean<F>(&mut self, current: &CurrentEpoch, mut f: F)
    where
        F: FnMut(T::Data),
    {
        let index = (current.now().0 - self.offset) as usize;
        let len = self.queue.len();

        for mut vec in self.queue.drain(..min(index, len)) {
            for value in vec.drain(..) {
                if is_valid(&value, current.now()) {
                    panic!(
                        "Value is valid until {:?}, current {:?}",
                        value.valid_until(),
                        current
                    );
                }
                f(value.dispose(current).unwrap_or_else(|_| unreachable!()));
            }
            self.clean_vecs.push(vec);
        }
        self.offset += index as u64;
    }
}

#[cfg(target_pointer_width = "64")]
#[derive(Debug)]
struct AtomicEpoch(AtomicUsize);
impl AtomicEpoch {
    fn new(epoch: Epoch) -> Self {
        AtomicEpoch(AtomicUsize::new(epoch.0 as usize))
    }

    /// Advance epoch if it less then specified.
    fn advance_to(&self, other: Epoch) {
        let value = other.0 as usize;
        if self.0.load(Ordering::Relaxed) < value {
            self.0.store(value as usize, Ordering::Relaxed); // TODO: Use `fetch_add`.
        }
    }

    fn epoch(&self) -> Epoch {
        Epoch(self.0.load(Ordering::Relaxed) as u64)
    }
}

#[cfg(not(target_pointer_width = "64"))]
const ERROR: () = "Your pointers are too small. Please try again with a more expensive computer.";