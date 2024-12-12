use core::{
    future::Future,
    marker::PhantomData,
    mem::{self, ManuallyDrop},
    ops::{Deref, DerefMut, Try},
    pin::Pin,
    ptr::NonNull,
    sync::atomic::AtomicUsize, 
    task::Waker,
};

use abs_sync::{
    cancellation::{CancelledToken, TrCancellationToken},
    sync_tasks::TrSyncTask,
};
use atomex::{StrictOrderings, TrAtomicFlags, TrCmpxchOrderings};
use atomic_sync::mutex::embedded::{MsbAsMutexSignal, SpinningMutexBorrowed};
use pincol::{
    linked_list::{PinnedList, PinnedSlot},
    x_deps::{abs_sync, atomex, atomic_sync},
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

#[repr(C)]
pub struct Glimpse<F, O = StrictOrderings>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    glimpse_st_: GlimpseState<O>,
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
            glimpse_st_: GlimpseState::new(),
            wake_queue_: PinnedList::new(),
            fut_or_out_: FutOrOut::new(future),
        }
    }

    pub fn try_peek(&self) -> Result<Option<&F::Output>, SnapshotError> {
        let r = self
            .spin_peek()
            .may_cancel_with(CancelledToken::pinned());
        match r {
            Result::Ok(x) => Result::Ok(Option::Some(x)),
            Result::Err(SnapshotError::Cancelled) => Result::Ok(Option::None),
            Result::Err(e) => Result::Err(e),
        }
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
        let cell = &mut this_mut.glimpse_st_.0;
        FutOrOutMutex::new(f_pin, cell)
    }

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

#[derive(Clone, Copy, Debug)]
pub enum SnapshotError {
    /// The glimpse future is pending
    Pending,

    /// Operation cancelled before its completion
    Cancelled,
}

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
        mut cancel: Pin<&mut C>,
    ) -> Result<&'a F::Output, SnapshotError> {
        let glimpse = self.0;
        let mutex = glimpse.mutex();
        loop {
            let opt_g = mutex.acquire().may_cancel_with(cancel.as_mut());
            let Option::Some(_g) = opt_g else {
                break Result::Err(SnapshotError::Cancelled);
            };
            let v = glimpse.glimpse_st_.value();
            if GlimpseState::<O>::expect_future_ready(v) {
                break Result::Ok(unsafe { glimpse.fut_or_out_.output_ref() });
            }
        }
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

pub(super) union FutOrOut<F>
where
    F: Future,
{
    future_: ManuallyDrop<F>,
    output_: ManuallyDrop<F::Output>,
}

impl<F> FutOrOut<F>
where
    F: Future,
{
    pub const fn new(future: F) -> Self {
        FutOrOut { future_: ManuallyDrop::new(future) }
    }

    pub unsafe fn pinned_future(self: Pin<&mut Self>) -> Pin<&mut F> {
        let this = unsafe { self. get_unchecked_mut() };
        unsafe { Pin::new_unchecked(this.future_.deref_mut()) }
    }

    pub unsafe fn output_ref(&self) -> &F::Output {
        self.output_.deref()
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

const K_MUTEX_FLAG: usize = 1usize << (usize::BITS - 1);
const K_READY_FLAG: usize = K_MUTEX_FLAG >> 1;
const K_USECNT_MASK: usize = K_READY_FLAG - 1;

impl<O> GlimpseState<O>
where
    O: TrCmpxchOrderings,
{
    const fn new() -> Self {
        GlimpseState(AtomicUsize::new(0usize), PhantomData)
    }

    pub fn expect_mutex_acquired(v: usize) -> bool {
        v & K_MUTEX_FLAG == K_MUTEX_FLAG
    }
    pub fn expect_mutex_released(v: usize) -> bool {
        !Self::expect_mutex_acquired(v)
    }
    pub fn desire_mutex_acquired(v: usize) -> usize {
        v | K_MUTEX_FLAG
    }
    pub fn desire_mutex_released(v: usize) -> usize {
        v & (!K_MUTEX_FLAG)
    }

    pub fn expect_future_ready(v: usize) -> bool {
        v & K_READY_FLAG == K_READY_FLAG
    }
    pub fn expect_future_pending(v: usize) -> bool {
        !Self::expect_future_ready(v)
    }
    pub fn desire_future_ready(v: usize) -> usize {
        v | K_READY_FLAG
    }

    pub fn read_use_count(v: usize) -> usize {
        v & K_USECNT_MASK
    }
    pub fn expect_use_cnt_incr_legal(v: usize) -> bool {
        Self::read_use_count(v) < K_USECNT_MASK
    }
    pub fn expect_use_cnt_decr_legal(v: usize) -> bool {
        Self::read_use_count(v) > 0
    }
}

#[cfg(test)]
mod tests_ {
    use super::*;

    #[test]
    fn glimpse_pinned_from_wake_queue_should_work() {
        use core::{
            future::Ready,
            mem::MaybeUninit,
            ptr,
        };
    
        let mut glimpse = unsafe {
            MaybeUninit::<Glimpse<Ready<()>>>::uninit().as_mut_ptr().read()
        };
        let p_queue = unsafe {
            Pin::new_unchecked(&mut glimpse.wake_queue_)
        };
        let p_glimpse = Glimpse::<Ready<()>>::glimpse_ptr_from_wake_queue_(p_queue);
        assert!(ptr::eq(
            unsafe { p_glimpse.as_ref() },
            &glimpse,
        ));
    }

    #[tokio::test]
    async fn demo() {
        use atomex::StrictOrderings;
        use crate::{x_deps::atomex, Glimpse};

        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::channel::<()>(1);
        let glimpse = Glimpse::<_, StrictOrderings>::new(rx.recv());
        let mut snapshot = glimpse.snapshot();
        assert!(matches!(
            snapshot.try_peek(),
            Result::Ok(Option::None)),
        );
        assert!(tx.try_send(()).is_ok());
        assert!(matches!(
            snapshot.peek_async().await,
            Result::Ok(_),
        ));
    }
}
