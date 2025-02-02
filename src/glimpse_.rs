use core::{
    fmt,
    future::Future,
    marker::{PhantomData, PhantomPinned},
    mem,
    ops::Try,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize},
    task::Waker,
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_sync::{
    cancellation::{CancelledToken, TrCancellationToken},
    sync_tasks::TrSyncTask,
};
use atomex::{StrictOrderings, TrAtomicFlags, TrCmpxchOrderings};
use atomic_sync::{
    mutex::embedded::{MsbAsMutexSignal, SpinningMutexBorrowed},
    x_deps::{abs_sync, atomex, pin_utils},
};

use crate::{
    fade_::FadeAsync,
    snapshot_::{AtomexSnapshotPtr, Snapshot},
};

pub(super) type FutOrOutMutex<'a, F, O> = SpinningMutexBorrowed<
    'a,
    Pin<&'a mut FutOrOut<F>>,
    AtomicUsize,
    MsbAsMutexSignal<usize>,
    O,
>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    Cancelled,
    StandingBy,
    GlimpseFaded,
    GlimpseMissing,
}

/// A glimpse captures the output of a future, and may spawn some snapshots.
#[repr(C)]
pub struct Glimpse<F, O = StrictOrderings>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    stat_: GlimpseState<O>,
    head_: AtomexSnapshotPtr<F, O>,
    fade_: Option<Waker>,
    fout_: FutOrOut<F>,
    _pin_: PhantomPinned,
}

impl<F, O> Glimpse<F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    pub const fn new(future: F) -> Self {
        let np = ptr::null_mut();
        Glimpse {
            stat_: GlimpseState::new(),
            head_: AtomexSnapshotPtr::new(AtomicPtr::new(np)),
            fade_: Option::None,
            fout_: FutOrOut::new(future),
            _pin_: PhantomPinned,
        }
    }

    pub fn try_peek(&self) -> Option<&F::Output> {
        self.spin_peek().may_cancel_with(CancelledToken::pinned())
    }

    pub fn spin_peek(&self) -> GlimpsePeekTask<'_, F, O> {
        GlimpsePeekTask(self)
    }

    pub fn snapshot(self: Pin<&mut Self>) -> Snapshot<F, O> {
        if let Option::Some(head) = self.head_snapshot_() {
            todo!()
        } else {
            Snapshot::head(self.as_ref().get_ref())
        }
    }

    /// To keep the glimpse alive until the future becomes ready and all the
    /// snapshots are dropped.
    pub fn fade_async(mut self: Pin<&mut Self>) -> FadeAsync<'_, F, O> {
        unsafe {
            self.as_mut()
                .get_unchecked_mut()
                .fade_ = Option::None;
        };
        FadeAsync::new(self)
    }

    pub(super) fn head_snapshot_(&self) -> Option<NonNull<Snapshot<F, O>>> {
        self.head_.load()
    }

    pub(super) fn mutex(&self) -> FutOrOutMutex<'_, F, O> {
        let this_mut = unsafe {
            let p = self as *const _ as *mut Self;
            let mut p = NonNull::new_unchecked(p);
            p.as_mut()
        };
        let f_pin = unsafe {
            Pin::new_unchecked(&mut this_mut.fout_)
        };
        let cell = &mut this_mut.stat_.0;
        FutOrOutMutex::new(f_pin, cell)
    }

    pub(super) fn state(&self) -> &GlimpseState<O> {
        &self.stat_
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

impl<F, O> Drop for Glimpse<F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let Result::Ok(head) = self.head_.try_reset() else {
            return;
        };
        let head = unsafe { head.as_ref() };
    }
}

impl<F, O> fmt::Debug for Glimpse<F, O>
where
    F: Future,
    F::Output: fmt::Debug,
    O: TrCmpxchOrderings,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Option::Some(x) = self.try_peek() {
            write!(f, "Glimpse({x:?}@{self:p})")
        } else {
            write!(f, "Glimpse@{self:p}")
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
pub struct GlimpsePeekTask<'a, F, O>(&'a Glimpse<F, O>)
where
    F: Future,
    O: TrCmpxchOrderings;

impl<'a, F, O> GlimpsePeekTask<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    pub const fn new(glimpse: &'a Glimpse<F, O>) -> Self {
        GlimpsePeekTask(&glimpse)
    }

    pub fn glimpse(&self) -> &Glimpse<F, O> {
        self.0
    }

    pub fn may_cancel_with<C: TrCancellationToken>(
        self,
        cancel: Pin<&mut C>,
    ) -> Option<&'a F::Output> {
        loop {
            let v = self.0.stat_.value();
            if StUtils::expect_future_ready(v) {
                let FutOrOut::Out(x) = &self.0.fout_ else {
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

impl<'a, F, O> TrSyncTask for GlimpsePeekTask<'a, F, O>
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
        GlimpsePeekTask::may_cancel_with(self, cancel)
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
        let state = &glimpse.stat_;
        let v = state.value();
        // a new glimpse has released mutex state.
        assert!(StUtils::expect_mutex_released(v));

        let f_mutex = glimpse.mutex();
        let acq_f = f_mutex.acquire();
        pin_mut!(acq_f);
        let g = acq_f.lock().wait();
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
        pin_mut!(glimpse);

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

        assert!(glimpse.fade_async().await.is_ok());
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
