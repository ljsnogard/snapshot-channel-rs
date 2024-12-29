use core::{
    borrow::Borrow,
    fmt,
    future::{Future, IntoFuture},
    marker::PhantomPinned,
    ops::{Deref, DerefMut, Try},
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize},
    task::{Context, Poll, Waker},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_sync::{
    cancellation::{
        CancelledToken, NonCancellableToken,
        TrCancellationToken, TrIntoFutureMayCancel,
    },
    sync_tasks::TrSyncTask,
};
use atomex::{AtomexPtr, StrictOrderings, TrAtomicFlags, TrCmpxchOrderings};
use atomic_sync::{
    rwlock::preemptive::SpinningRwLockBorrowed,
    x_deps::{abs_sync, atomex, pin_utils}
};

use super::glimpse_::{
    FutOrOut, FutOrOutProj,
    Glimpse, SnapshotError, StUtils,
};

pub(super) type AtomexGlimpsePtr<F, O> =
    AtomexPtr<Glimpse<F, O>, AtomicPtr<Glimpse<F, O>>, O>;

pub(super) type AtomexSnapshotPtr<F, O> =
    AtomexPtr<Snapshot<F, O>, AtomicPtr<Snapshot<F, O>>, O>;

pub(super) type SnapshotRwlock<'a, F, O> =
    SpinningRwLockBorrowed<'a, Pin<&'a mut Snapshot<F, O>>, AtomicUsize, O>;

/// To capture the output of the glimpse's future.
pub struct Snapshot<F, O = StrictOrderings>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    rwlock_stat_: AtomicUsize,
    glimpse_ptr_: AtomexGlimpsePtr<F, O>,
    prev_: AtomexSnapshotPtr<F, O>,
    next_: AtomexSnapshotPtr<F, O>,
    wake_: Option<Waker>,
    _pin_: PhantomPinned,
}

impl<F, O> Snapshot<F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(glimpse: *const Glimpse<F, O>) -> Self {
        Snapshot {
            rwlock_stat_: AtomicUsize::new(0),
            glimpse_ptr_: AtomexGlimpsePtr::new(AtomicPtr::new(glimpse as _)),
            prev_: AtomexSnapshotPtr::new(AtomicPtr::new(ptr::null_mut())),
            next_: AtomexSnapshotPtr::new(AtomicPtr::new(ptr::null_mut())),
            wake_: Option::None,
            _pin_: PhantomPinned,
        }
    }

    #[inline]
    pub fn try_peek(&self) -> Result<Option<&F::Output>, SnapshotError> {
        let cancel = CancelledToken::pinned();
        match self.spin_peek().may_cancel_with(cancel) {
            Result::Ok(x) => Result::Ok(Option::Some(x)),
            Result::Err(SnapshotError::Cancelled) => Result::Ok(Option::None),
            Result::Err(e) => Result::Err(e),
        }
    }

    #[inline]
    pub fn spin_peek(&self) -> SnapshotPeekTask<'_, F, O> {
        SnapshotPeekTask::new(self)
    }

    #[inline]
    pub fn peek_async(self: Pin<&mut Self>) -> PeekAsync<'_, F, O> {
        PeekAsync(self)
    }

    pub(super) fn rwlock_(&self) -> SnapshotRwlock<'_, F, O> {
        unsafe {
            let this = self as *const Self as *mut Self;
            let data = Pin::new_unchecked(&mut (*this));
            let cell = &mut (*this).rwlock_stat_;
            SnapshotRwlock::new(data, cell)
        }
    }

    pub(super) fn try_find_head_<C: TrCancellationToken>(
        &self,
        cancel: Pin<&mut C>,
    ) -> Result<&Self, SnapshotError> {
        let mut curr = self;
        loop {
            let curr_rwlock = curr.rwlock_();
            let curr_acq = curr_rwlock.acquire();
            pin_mut!(curr_acq);
            let opt_g = curr_acq.try_read();
            let Option::Some(g) = opt_g else {
                if cancel.is_cancelled() {
                    break Result::Err(SnapshotError::Cancelled);
                } else {
                    continue;
                }
            };
            let opt_prev = (*g).as_ref().get_ref().prev_.load();
            let Option::Some(prev) = opt_prev else {
                break Result::Ok(curr);
            };
            curr = unsafe { prev.as_ref() };
        }
    }
}

impl<F, O> Clone for Snapshot<F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    fn clone(&self) -> Self {
        Snapshot::new(ptr::null_mut())
    }
}

impl<F, O> fmt::Debug for Snapshot<F, O>
where
    F: Future,
    F::Output: fmt::Debug,
    O: TrCmpxchOrderings,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prev = self.prev_.pointer();
        let next = self.next_.pointer();
        write!(f, "Snapshot[{self:p}, prev({prev:p}), next({next:p})")?;
        if let Option::Some(glimpse) = self.glimpse_ptr_.load() {
            let x = unsafe { glimpse.as_ref() };
            write!(f, ", glimpse({x:?})]")?;
        }
        write!(f, "]")
    }
}

unsafe impl<F, O> Send for Snapshot<F, O>
where
    F: Send + Sync + Future,
    O: TrCmpxchOrderings,
{}

unsafe impl<F, O> Sync for Snapshot<F, O>
where
    F: Send + Sync + Future,
    O: TrCmpxchOrderings,
{}

pub struct SnapshotPeekTask<'a, F, O>(&'a Snapshot<F, O>)
where
    F: Future,
    O: TrCmpxchOrderings;

impl<'a, F, O> SnapshotPeekTask<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    pub const fn new(snapshot: &'a Snapshot<F, O>) -> Self {
        SnapshotPeekTask(snapshot)
    }

    pub fn may_cancel_with<C: TrCancellationToken>(
        self,
        mut cancel: Pin<&mut C>,
    ) -> Result<&'a F::Output, SnapshotError> {
        let head = self.0.try_find_head_(cancel.as_mut())?;
        let rwlock = head.rwlock_();
        let acq = rwlock.acquire();
        pin_mut!(acq);
        let opt_g = acq.read().may_cancel_with(cancel.as_mut());
        let Option::Some(g) = opt_g else {
            return Result::Err(SnapshotError::Cancelled);
        };
        let Option::Some(glimpse) = g.glimpse_ptr_.load() else {
            return Result::Err(SnapshotError::GlimpseMissing);
        };
        // Safe because by design glimpse has to reset all glimpse_ptr_
        // before it is dropped.
        let glimpse = unsafe { glimpse.as_ref() };
        self.0.glimpse_ptr_.store(glimpse as *const _ as *mut Glimpse<F, O>);

        let opt_x = glimpse.spin_peek().may_cancel_with(cancel);
        let Option::Some(x) = opt_x else {
            return Result::Err(SnapshotError::Cancelled);
        };
        Result::Ok(x)
    }

    #[inline]
    pub fn wait(self) -> &'a F::Output {
        TrSyncTask::wait(self)
    }
}

impl<'a, F, O> TrSyncTask for SnapshotPeekTask<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    type Output = &'a F::Output;

    #[inline]
    fn may_cancel_with<C: TrCancellationToken>(
        self,
        cancel: Pin<&mut C>,
    ) -> impl Try<Output = Self::Output> {
        SnapshotPeekTask::may_cancel_with(self, cancel)
    }
}

pub struct PeekAsync<'a, F, O>(Pin<&'a mut Snapshot<F, O>>)
where
    F: Future,
    O: TrCmpxchOrderings;

impl<'a, F, O> PeekAsync<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    pub fn may_cancel_with<C: TrCancellationToken>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> PeekFuture<'a, C, F, O> {
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, F, O> IntoFuture for PeekAsync<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    type IntoFuture = PeekFuture<'a, NonCancellableToken, F, O>;
    type Output = Option<&'a F::Output>;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        PeekAsync::may_cancel_with(self, cancel)
    }
}

impl<'a, F, O> TrIntoFutureMayCancel<'a> for PeekAsync<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    type MayCancelOutput = <Self as IntoFuture>::Output;

    #[inline]
    fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> impl Future<Output = Self::MayCancelOutput>
    where
        C: TrCancellationToken,
    {
        PeekAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct PeekFuture<'a, C, F, O>
where
    C: TrCancellationToken,
    F: Future,
    O: TrCmpxchOrderings,
{
    snapshot_: Pin<&'a mut Snapshot<F, O>>,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, F, O> PeekFuture<'a, C, F, O>
where
    C: TrCancellationToken,
    F: Future,
    O: TrCmpxchOrderings,
{
    pub const fn new(
        snapshot: Pin<&'a mut Snapshot<F, O>>,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        PeekFuture {
            snapshot_: snapshot,
            cancel_: cancel,
        }
    }
}

impl<'a, C, F, O> Future for PeekFuture<'a, C, F, O>
where
    C: TrCancellationToken,
    F: Future,
    O: TrCmpxchOrderings,
{
    type Output = Option<&'a F::Output>;

    /// ## Possible situations:
    /// 1. Slot not queued: then we simply enqueue the slot, and poll the 
    ///     future of cancellation for later wake up;
    /// 2. Slot has enqueued but not the first one to poll the future stored
    ///     within the `Glimpse`: just return pending;
    /// 3. Slot has enqueued and become the first one to poll the future stored
    ///     within the `Glimpse`:;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut snapshot_ptr = unsafe {
            let p = this.snapshot_.as_mut().get_unchecked_mut();
            NonNull::new_unchecked(p)
        };
        let snapshot_ref = unsafe { snapshot_ptr.as_ref() };
        let glimpse: &'a Glimpse<F, O> = snapshot_ref.glimpse_ref_.borrow();
        let q_mutex = glimpse.wake_queue().mutex();
        loop {
            let opt_q = snapshot_ref.waker_slot_.attached_list();
            if let Option::Some(q) = opt_q {
                assert!(ptr::eq(q, glimpse.wake_queue()));

                let v = glimpse.state().value();
                if StUtils::expect_future_ready(v) {
                    debug_assert!(!StUtils::expect_future_pending(v));
                    let opt = glimpse.try_peek();
                    assert!(opt.is_some());
                    #[cfg(test)]
                    log::trace!(
                        "[PeekFuture::poll] {:p} Situation 2: ready",
                        snapshot_ref,
                    );
                    break Poll::Ready(opt);
                }
                if StUtils::expect_future_pending(v) {
                    debug_assert!(!StUtils::expect_future_ready(v));
                    #[cfg(test)]
                    log::trace!(
                        "[PeekFuture::poll] {:p} Situation 2: pending",
                        snapshot_ref,
                    );
                    break Poll::Pending;
                }
                if this.cancel_.is_cancelled() {
                    #[cfg(test)]
                    log::trace!(
                        "[PeekFuture::poll] {:p} Situation 2: cancelled",
                        snapshot_ref,
                    );
                    break Poll::Ready(Option::None);
                }
                let f_mutex = glimpse.mutex();
                let acq_f = f_mutex.acquire();
                pin_mut!(acq_f);
                let Option::Some(mut f_guard) = acq_f.try_lock()
                else {
                    continue;
                };
                // Situation 3: The first time to poll the future in glimpse.
                let mut fut_or_out = (*f_guard).as_mut();
                let FutOrOutProj::Fut(fut) = fut_or_out.as_mut().project()
                else {
                    unreachable!("[PeekFuture::poll]")
                };
                let Poll::Ready(x) = fut.poll(cx) else {
                    let expect = |v|
                        !StUtils::expect_future_pending(v) &&
                        !StUtils::expect_future_ready(v);
                    let desire = StUtils::desire_future_pending;
                    let r = glimpse
                        .state()
                        .try_spin_compare_exchange_weak(expect, desire);
                    assert!(r.is_succ());
                    #[cfg(test)]
                    log::trace!(
                        "[PeekFuture::poll] {:p} Situation 3: pending #1",
                        snapshot_ref,
                    );
                    break Poll::Pending;
                };
                let p = unsafe { fut_or_out.get_unchecked_mut() };
                *p = FutOrOut::<F>::Out(x);
                let expect = |v|
                    !StUtils::expect_future_pending(v) &&
                    !StUtils::expect_future_ready(v);
                let desire = StUtils::desire_future_ready;
                let r = glimpse
                    .state()
                    .try_spin_compare_exchange_weak(expect, desire);
                assert!(r.is_succ());
                glimpse.wake_all();

                #[cfg(test)]
                log::trace!(
                    "[PeekFuture::poll] {:p} Situation 3: pending #2",
                    snapshot_ref,
                );
                break Poll::Pending;
            } else {
                // Situation 1: Slot not queued.
                let opt_out = unsafe { snapshot_ptr.as_mut().try_peek() };
                if opt_out.is_some() {
                    #[cfg(test)]
                    log::trace!(
                        "[PeekFuture::poll] {:p} Situation 1: ready",
                        snapshot_ref,
                    );
                    break Poll::Ready(opt_out);
                } else {
                    let fut_cancel = this
                        .cancel_
                        .as_mut()
                        .cancellation()
                        .into_future();
                    pin_mut!(fut_cancel);
                    if fut_cancel.poll(cx).is_ready() {
                        #[cfg(test)]
                        log::trace!(
                            "[PeekFuture::poll] {:p} Situation 1: cancelled #1",
                            snapshot_ref,
                        );
                        break Poll::Ready(Option::None)
                    };
                }
                let acq_q = q_mutex.acquire();
                pin_mut!(acq_q);
                let opt_g = acq_q
                    .lock()
                    .may_cancel_with(this.cancel_.as_mut());
                let Option::Some(mut g) = opt_g else {
                    #[cfg(test)]
                    log::trace!(
                        "[PeekFuture::poll] {:p} Situation 1: cancelled #2",
                        snapshot_ref,
                    );
                    break Poll::Ready(Option::None)
                };
                let mut slot = unsafe {
                    Pin::new_unchecked(&mut snapshot_ptr.as_mut().waker_slot_)
                };
                let opt_waker = slot.as_mut().data_pinned().get_mut();
                let _ = opt_waker.replace(cx.waker().clone());
                let r = (*g).as_mut().push_tail(slot.as_mut());
                assert!(r.is_ok());
            }
        }
    }
}
