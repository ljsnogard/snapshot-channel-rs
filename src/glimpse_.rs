use core::{
    future::Future,
    marker::PhantomData,
    mem,
    ops::Try,
    pin::Pin,
    ptr::NonNull,
    sync::atomic::AtomicUsize, 
    task::Waker,
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_sync::{
    cancellation::{CancelledToken, TrCancellationToken},
    sync_tasks::TrSyncTask,
};
use atomex::{StrictOrderings, TrAtomicFlags, TrCmpxchOrderings};
use atomic_sync::mutex::embedded::{MsbAsMutexSignal, SpinningMutexBorrowed};
use pincol::{
    linked_list::{PinnedList, PinnedSlot},
    x_deps::{abs_sync, atomex, atomic_sync, pin_utils},
};

use crate::Snapshot;

pub(super) type WakeQueue<O> = PinnedList<Option<Waker>, O>;
pub(super) type WakerSlot<O> = PinnedSlot<Option<Waker>, O>;

pub(super) type FutOrOutMutex<'a, F, O> = SpinningMutexBorrowed<
    'a,
    Pin<&'a mut FutOrOut<F>>,
    AtomicUsize,
    MsbAsMutexSignal<usize>,
    O,
>;

/// A glimpse captures the output of a future, and may spawn some snapshots.
#[repr(C)]
pub struct Glimpse<F, O = StrictOrderings>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    stat_flags_: GlimpseState<O>,
    wake_queue_: WakeQueue<O>,
    fut_or_out_: FutOrOut<F>,
}

impl<F, O> Glimpse<F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    pub const fn new(future: F) -> Self {
        Glimpse {
            stat_flags_: GlimpseState::new(),
            wake_queue_: PinnedList::new(),
            fut_or_out_: FutOrOut::new(future),
        }
    }

    pub fn try_peek(&self) -> Option<&F::Output> {
        self.spin_peek().may_cancel_with(CancelledToken::pinned())
    }

    pub fn spin_peek(&self) -> PeekTask<'_, F, O> {
        PeekTask(self)
    }

    pub fn snapshot(&self) -> Snapshot<&Self, F, O> {
        Snapshot::new(self)
    }

    pub(super) const fn wake_queue(&self) -> &WakeQueue<O> {
        &self.wake_queue_
    }

    pub(super) fn mutex(&self) -> FutOrOutMutex<'_, F, O> {
        let this_mut = unsafe {
            let p = self as *const _ as *mut Self;
            let mut p = NonNull::new_unchecked(p);
            p.as_mut()
        };
        let f_pin = unsafe {
            Pin::new_unchecked(&mut this_mut.fut_or_out_)
        };
        let cell = &mut this_mut.stat_flags_.0;
        FutOrOutMutex::new(f_pin, cell)
    }

    pub(super) fn state(&self) -> &GlimpseState<O> {
        &self.stat_flags_
    }

    pub(super) fn wake_all(&self) {
        fn wake_<Ord: TrCmpxchOrderings>(
            slot: Pin<&mut WakerSlot<Ord>>,
        ) -> bool{
            let Option::Some(waker) = slot.data_pinned().take() else {
                return true;
            };
            waker.wake();
            true
        }
        let mutex = self.wake_queue_.mutex();
        let acq = mutex.acquire();
        pin_mut!(acq);
        let mut g = acq.lock().wait();
        let queue_pin = (*g).as_mut();
        let _ = queue_pin.clear(wake_);
    }

    #[allow(unused)]
    pub(super) fn glimpse_ptr_from_wake_queue_(
        queue: Pin<&mut WakeQueue<O>>,
    ) -> NonNull<Self> {
        let offset = mem::offset_of!(Self, wake_queue_);
        unsafe {
            let p = queue.get_unchecked_mut() as *mut WakeQueue<O> as *mut u8;
            let p_glimpse = p.byte_sub(offset) as *mut Self;
            NonNull::new_unchecked(p_glimpse)
        }
    }
}

unsafe impl<F, O> Send for Glimpse<F, O>
where
    F: Send + Sync + Future,
    O: TrCmpxchOrderings,
{}

unsafe impl<F, O> Sync for Glimpse<F, O>
where
    F: Send + Sync + Future,
    O: TrCmpxchOrderings,
{}

/// The synchronized task to loop and check if the future held by the `Glimpse`
/// is ready and peek the reference of its output.
pub struct PeekTask<'a, F, O>(&'a Glimpse<F, O>)
where
    F: Future,
    O: TrCmpxchOrderings;

impl<'a, F, O> PeekTask<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    pub fn glimpse(&self) -> &Glimpse<F, O> {
        self.0
    }

    pub fn may_cancel_with<C: TrCancellationToken>(
        self,
        cancel: Pin<&mut C>,
    ) -> Option<&'a F::Output> {
        loop {
            let v = self.0.stat_flags_.value();
            if StUtils::expect_future_ready(v) {
                let FutOrOut::Out(x) = &self.0.fut_or_out_ else {
                    unreachable!()
                };
                break Option::Some(x);
            }
            if cancel.is_cancelled() {
                break Option::None
            }
        }
    }

    #[inline]
    pub fn wait(self) -> &'a F::Output {
        TrSyncTask::wait(self)
    }
}

impl<'a, F, O> TrSyncTask for PeekTask<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    type Output = &'a F::Output;

    #[inline]
    fn may_cancel_with<C>(
        self,
        cancel: Pin<&mut C>,
    ) -> impl Try<Output = Self::Output>
    where
        C: TrCancellationToken
    {
        PeekTask::may_cancel_with(self, cancel)
    }
}

#[pin_project(project = FutOrOutProj)]
pub(super) enum FutOrOut<F>
where
    F: Future,
{
    Fut(#[pin]F),
    Out(F::Output),
}

impl<F> FutOrOut<F>
where
    F: Future,
{
    pub const fn new(future: F) -> Self {
        FutOrOut::Fut(future)
    }
}

pub(super) struct StUtils;

impl StUtils {
    pub const K_MUTEX_FLAG: usize = 1usize << (usize::BITS - 1);
    pub const K_PENDING_FLAG: usize = Self::K_MUTEX_FLAG >> 1;
    pub const K_READY_FLAG: usize = Self::K_PENDING_FLAG >> 1;

    #[allow(dead_code)]
    pub fn expect_mutex_acquired(v: usize) -> bool {
        v & Self::K_MUTEX_FLAG == Self::K_MUTEX_FLAG
    }
    #[allow(dead_code)]
    pub fn expect_mutex_released(v: usize) -> bool {
        !Self::expect_mutex_acquired(v)
    }
    #[allow(dead_code)]
    pub fn desire_mutex_acquired(v: usize) -> usize {
        v | Self::K_MUTEX_FLAG
    }
    #[allow(dead_code)]
    pub fn desire_mutex_released(v: usize) -> usize {
        v & (!Self::K_MUTEX_FLAG)
    }

    pub fn expect_future_ready(v: usize) -> bool {
        v & Self::K_READY_FLAG == Self::K_READY_FLAG
    }
    pub fn expect_future_pending(v: usize) -> bool {
        v & Self::K_PENDING_FLAG == Self::K_PENDING_FLAG
    }
    pub fn desire_future_ready(v: usize) -> usize {
        v | Self::K_READY_FLAG & (!Self::K_PENDING_FLAG)
    }
    pub fn desire_future_pending(v: usize) -> usize {
        v | Self::K_PENDING_FLAG
    }
}

pub(super) struct GlimpseState<O>(AtomicUsize, PhantomData<O>)
where
    O: TrCmpxchOrderings;

impl<O> AsRef<AtomicUsize> for GlimpseState<O>
where
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &AtomicUsize {
        &self.0
    }
}

impl<O> TrAtomicFlags<usize, O> for GlimpseState<O>
where
    O: TrCmpxchOrderings,
{}

impl<O> GlimpseState<O>
where
    O: TrCmpxchOrderings,
{
    const fn new() -> Self {
        GlimpseState(AtomicUsize::new(0usize), PhantomData)
    }
}

#[cfg(test)]
mod tests_ {
    use core::future;
    use pincol::x_deps::pin_utils::pin_mut;

    use super::*;

    #[test]
    fn glimpse_ptr_from_wake_queue_should_work() {
        use core::{future::Ready, mem::MaybeUninit, ptr};

        let mut m = MaybeUninit::<Glimpse<Ready<()>>>::uninit();
        let glimpse = unsafe { m.as_mut_ptr().as_mut().unwrap() };
        let p_queue = unsafe {
            Pin::new_unchecked(&mut glimpse.wake_queue_)
        };
        let p_glimpse = Glimpse::<Ready<()>>::glimpse_ptr_from_wake_queue_(p_queue);
        assert!(ptr::eq(
            unsafe { p_glimpse.as_ref() },
            glimpse,
        ));
    }

    #[test]
    fn glimpse_state_msb_should_change_on_mutex_acq() {
        let glimpse = Glimpse::<_, StrictOrderings>
            ::new(future::pending::<()>());
        let state = &glimpse.stat_flags_;
        let v = state.value();
        // a new glimpse has released mutex state.
        assert!(StUtils::expect_mutex_released(v));

        let f_mutex = glimpse.mutex();
        let acq = f_mutex.acquire();
        pin_mut!(acq);
        let g = acq.lock().wait();
        let v = state.value();
        assert!(StUtils::expect_mutex_acquired(v));
        drop(g);
        let v = state.value();
        assert!(StUtils::expect_mutex_released(v));
    }

    #[tokio::test]
    async fn demo() {
        use pin_utils::pin_mut;

        use atomex::StrictOrderings;
        use crate::{x_deps::{atomex, pin_utils}, Glimpse};

        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::channel::<()>(1);
        let glimpse = Glimpse::<_, StrictOrderings>::new(rx.recv());
        let snapshot = glimpse.snapshot();
        let snapshot_cloned = snapshot.clone();

        assert!(snapshot.try_peek().is_none());
        assert!(snapshot_cloned.try_peek().is_none());

        pin_mut!(snapshot);
        pin_mut!(snapshot_cloned);

        assert!(tx.try_send(()).is_ok());
        assert!(snapshot.as_mut().peek_async().await.is_some());

        // Cloned snapshot should observe the same result
        assert!(snapshot_cloned.peek_async().await.is_some());

        // Multiple times of peek_async is legal and idempotent
        assert!(snapshot.peek_async().await.is_some());
    }

    #[tokio::test]
    async fn future_should_properly_drop() {
        use std::{sync::Arc, task::*};

        use super::*;

        struct DemoFut(Arc<()>);

        impl Future for DemoFut {
            type Output = usize;

            fn poll(
                self: Pin<&mut Self>,
                _x: &mut Context<'_>,
            ) -> Poll<Self::Output> {
                Poll::Ready(Arc::strong_count(&self.as_ref().get_ref().0))
            }
        }

        let detect = Arc::new(());
        assert_eq!(Arc::strong_count(&detect), 1);

        let future = DemoFut(detect.clone());
        {
            let glimpse = Glimpse::<_, StrictOrderings>::new(future);
            assert_eq!(Arc::strong_count(&detect), 2);
            let snapshot = glimpse.snapshot();
            pin_mut!(snapshot);
            let c = snapshot.peek_async().await.unwrap();
            assert_eq!(*c, 2);
        }
        assert_eq!(Arc::strong_count(&detect), 1);
    }
}
