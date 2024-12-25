use core::{
    borrow::Borrow,
    future::{Future, IntoFuture},
    marker::PhantomData,
    pin::Pin,
    ptr::{self, NonNull},
    task::{Context, Poll},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_sync::cancellation::{
    NonCancellableToken, TrCancellationToken, TrIntoFutureMayCancel,
};
use atomex::{StrictOrderings, TrAtomicFlags, TrCmpxchOrderings};
use pincol::x_deps::{abs_sync, atomex, pin_utils};

use crate::glimpse_::FutOrOutProj;
use super::glimpse_::{FutOrOut, Glimpse, StUtils, PeekTask, WakerSlot};

/// The viewer of the channel output
pub struct Snapshot<B, F, O = StrictOrderings>
where
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    glimpse_ref_: B,
    waker_slot_: WakerSlot<O>,
    _unused_f_: PhantomData<NonNull<Glimpse<F, O>>>,
}

impl<B, F, O> Snapshot<B, F, O>
where
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    pub const fn new(glimpse: B) -> Self {
        Snapshot {
            glimpse_ref_: glimpse,
            waker_slot_: WakerSlot::new(Option::None),
            _unused_f_: PhantomData,
        }
    }

    #[inline]
    pub fn glimpse(&self) -> &Glimpse<F, O> {
        self.glimpse_ref_.borrow()
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        let v = self.glimpse().state().value();
        StUtils::expect_future_ready(v)
    }

    #[inline]
    pub fn try_peek(&self) -> Option<&F::Output> {
        self.glimpse().try_peek()
    }

    #[inline]
    pub fn spin_peek(&self) -> PeekTask<'_, F, O> {
        self.glimpse().spin_peek()
    }

    pub fn peek_async(mut self: Pin<&mut Self>) -> PeekAsync<'_, B, F, O> {
        let mut this_ptr = unsafe {
            NonNull::new_unchecked(self.as_mut().get_unchecked_mut())
        };
        let slot_ref = unsafe { &this_ptr.as_ref().waker_slot_ };
        if let Option::Some(q) = slot_ref.attached_list() {
            let mut slot_ptr = unsafe {
                let p = &mut this_ptr.as_mut().waker_slot_;
                NonNull::new_unchecked(p)
            };
            let mutex = q.mutex();
            let acq = mutex.acquire();
            pin_mut!(acq);
            let mut g = acq.lock().wait();
            let slot_pin = unsafe { Pin::new_unchecked(slot_ptr.as_mut()) };
            let Option::Some(mut cursor) = (*g).as_mut().find(slot_pin) else {
                unreachable!()
            };
            assert!(cursor.try_detach());
            let slot_mut = unsafe {
                // Safe because the slot is detached, and no alias
                &mut this_ptr.as_mut().waker_slot_
            };
            *slot_mut = WakerSlot::new(Option::None);
        }
        PeekAsync(unsafe { Pin::new_unchecked(self.get_unchecked_mut()) })
    }
}

impl<B, F, O> Clone for Snapshot<B, F, O>
where
    B: Clone + Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    fn clone(&self) -> Self {
        Snapshot::new(self.glimpse_ref_.clone())
    }
}

unsafe impl<B, F, O> Send for Snapshot<B, F, O>
where
    B: Clone + Borrow<Glimpse<F, O>>,
    F: Send + Sync + Future,
    O: TrCmpxchOrderings,
{}

unsafe impl<B, F, O> Sync for Snapshot<B, F, O>
where
    B: Clone + Borrow<Glimpse<F, O>>,
    F: Send + Sync + Future,
    O: TrCmpxchOrderings,
{}

pub struct PeekAsync<'a, B, F, O>(Pin<&'a mut Snapshot<B, F, O>>)
where
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings;

impl<'a, B, F, O> PeekAsync<'a, B, F, O>
where
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    pub fn may_cancel_with<C: TrCancellationToken>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> PeekFuture<'a, C, B, F, O> {
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, F, O> IntoFuture for PeekAsync<'a, B, F, O>
where
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    type IntoFuture = PeekFuture<'a, NonCancellableToken, B, F, O>;
    type Output = Option<&'a F::Output>;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        PeekAsync::may_cancel_with(self, cancel)
    }
}

impl<'a, B, F, O> TrIntoFutureMayCancel<'a> for PeekAsync<'a, B, F, O>
where
    B: Borrow<Glimpse<F, O>>,
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
pub struct PeekFuture<'a, C, B, F, O>
where
    C: TrCancellationToken,
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    snapshot_: Pin<&'a mut Snapshot<B, F, O>>,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, B, F, O> PeekFuture<'a, C, B, F, O>
where
    C: TrCancellationToken,
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    pub const fn new(
        snapshot: Pin<&'a mut Snapshot<B, F, O>>,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        PeekFuture {
            snapshot_: snapshot,
            cancel_: cancel,
        }
    }
}

impl<'a, C, B, F, O> Future for PeekFuture<'a, C, B, F, O>
where
    C: TrCancellationToken,
    B: Borrow<Glimpse<F, O>>,
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
