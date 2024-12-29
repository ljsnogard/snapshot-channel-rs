use core::{
    future::{Future, IntoFuture},
    pin::Pin,
    task::{Context, Poll, Waker},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_sync::cancellation::{
    NonCancellableToken, TrCancellationToken, TrIntoFutureMayCancel,
};
use atomex::{StrictOrderings, TrAtomicFlags, TrCmpxchOrderings};
use atomic_sync::{
    mutex::embedded::{MsbAsMutexSignal, SpinningMutexBorrowed},
    x_deps::{abs_sync, atomex, pin_utils},
};

use crate::Glimpse;

pub struct FadeAsync<'a, F, O>(Pin<&'a mut Glimpse<F, O>>)
where
    F: Future,
    O: TrCmpxchOrderings;

impl<'a, F, O> FadeAsync<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    pub const fn new(glimpse: Pin<&'a mut Glimpse<F, O>>) -> Self {
        FadeAsync(glimpse)
    }

    pub fn may_cancel_with<C: TrCancellationToken>(
        self,
        cancel: Pin<&mut C>,
    ) -> FadeFuture<'a, C, F, O> {
        todo!()
    }
}

impl<'a, F, O> IntoFuture for FadeAsync<'a, F, O>
where
    F: Future,
    O: TrCmpxchOrderings,
{
    type Output = <Self::IntoFuture as Future>::Output;
    type IntoFuture = FadeFuture<'a, NonCancellableToken, F, O>;

    fn into_future(self) -> Self::IntoFuture {
        todo!()
    }
}

impl<'a, F, O> TrIntoFutureMayCancel<'a> for FadeAsync<'a, F, O>
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
        FadeAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct FadeFuture<'a, C, F, O>
where
    C: TrCancellationToken,
    F: Future,
    O: TrCmpxchOrderings,
{
    glimpse_: Pin<&'a mut Glimpse<F, O>>,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, F, O> Future for FadeFuture<'a, C, F, O>
where
    C: TrCancellationToken,
    F: Future,
    O: TrCmpxchOrderings,
{
    type Output = Option<&'a F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        todo!()
    }
}
