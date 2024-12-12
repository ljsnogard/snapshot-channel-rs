use core::{
    borrow::Borrow,
    future::{Future, IntoFuture},
    marker::PhantomData,
    ops::Try,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll, Waker},
};

use abs_sync::cancellation::{
    NonCancellableToken, TrCancellationToken, TrIntoFutureMayCancel,
};
use atomex::{StrictOrderings, TrCmpxchOrderings};
use pincol::x_deps::{abs_sync, atomex};

use crate::{Glimpse, SnapshotError};
use super::glimpse_::{WakeQueue, WakerSlot, PeekTask};

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
    pub(super) const fn new(glimpse: B) -> Self {
        Snapshot {
            glimpse_ref_: glimpse,
            waker_slot_: WakerSlot::new(Option::None),
            _unused_f_: PhantomData,
        }
    }

    pub fn try_peek(&mut self) -> Result<Option<&F::Output>, SnapshotError> {
        self.glimpse_ref_.borrow().try_peek()
    }

    pub fn spin_peek(&mut self) -> PeekTask<'_, F, O> {
        self.glimpse_ref_.borrow().spin_peek()
    }

    pub fn peek_async(&mut self) -> PeekAsync<'_, B, F, O> {
        PeekAsync(unsafe { Pin::new_unchecked(self) })
    }
}

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
    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> PeekFuture<'a, C, B, F, O>
    where
        C: TrCancellationToken,
    {
        PeekFuture {
            glimpse_: self.0,
            cancel_: cancel,
        }
    }
}

impl<'a, B, F, O> IntoFuture for PeekAsync<'a, B, F, O>
where
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    type IntoFuture = PeekFuture<'a, NonCancellableToken, B, F, O>;
    type Output = Result<&'a F::Output, SnapshotError>;

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
        C: TrCancellationToken
    {
        PeekAsync::may_cancel_with(self, cancel)
    }
}

pub struct PeekFuture<'a, C, B, F, O>
where
    C: TrCancellationToken,
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    glimpse_: Pin<&'a mut Snapshot<B, F, O>>,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, B, F, O> Future for PeekFuture<'a, C, B, F, O>
where
    C: TrCancellationToken,
    B: Borrow<Glimpse<F, O>>,
    F: Future,
    O: TrCmpxchOrderings,
{
    type Output = Result<&'a F::Output, SnapshotError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        todo!()
    }
}