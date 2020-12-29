use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::FutureExt;

pub struct SelectReady<'a, K, Fut> {
    inner: &'a mut HashMap<K, Fut>,
}

impl<'a, K, Fut: Unpin> Unpin for SelectReady<'a, K, Fut> {}

pub fn select_ready<'a, F, K>(v: &'a mut HashMap<K, F>) -> SelectReady<'a, K, F>
where
    K: Eq + std::hash::Hash,
    F: Future + Unpin,
{
    SelectReady { inner: v }
}

impl<'a, K: Clone + Eq + std::hash::Hash, Fut: Future + Unpin> Future for SelectReady<'a, K, Fut> {
    type Output = Option<(Fut::Output, K)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // we voluntarily keep returning Pending instead of Ready(None) to be able to select! on this
        //if self.inner.len() == 0 {
        //	return Poll::Ready(None);
        //}
        let item = self
            .inner
            .iter_mut()
            .find_map(|(i, f)| match (*f).poll_unpin(cx) {
                Poll::Pending => None,
                Poll::Ready(e) => Some((i, e)),
            });
        match item {
            Some((idx, res)) => {
                let idx = idx.clone();
                self.inner.remove(&idx);
                return Poll::Ready(Some((res, idx)));
            }
            None => return Poll::Pending,
        }
    }
}

pub struct SelectVec<'a, Fut> {
    inner: &'a mut Vec<Fut>,
}

impl<'a, Fut: Unpin> Unpin for SelectVec<'a, Fut> {}

pub fn select_vec<'a, F>(v: &'a mut Vec<F>) -> SelectVec<'a, F>
where
    F: Future + Unpin,
{
    SelectVec { inner: v }
}

impl<'a, Fut: Future + Unpin> Future for SelectVec<'a, Fut> {
    type Output = Option<Fut::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // we voluntarily keep returning Pending instead of Ready(None) to be able to select! on this
        //if self.inner.len() == 0 {
        //	return Poll::Ready(None);
        //}
        let item = self
            .inner
            .iter_mut()
            .enumerate()
            .find_map(|(i, f)| match (*f).poll_unpin(cx) {
                Poll::Pending => None,
                Poll::Ready(e) => Some((i, e)),
            });
        match item {
            Some((idx, res)) => {
                self.inner.swap_remove(idx);
                return Poll::Ready(Some(res));
            }
            None => return Poll::Pending,
        }
    }
}
