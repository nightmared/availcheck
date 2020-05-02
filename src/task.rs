//use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::future::Future;
use std::pin::Pin;
/*
use std::marker::PhantomData;

use tokio::sync::mpsc::{channel, Sender, Receiver};
use tokio::sync::Mutex;
use futures::future::TryFuture;
use futures::future::TryFutureExt;
use futures::future::FutureExt;
*/

/*
// a slightly modified version of SelectOk in futures_util
pub struct SelectOk<Fut> {
	inner: HashMap<usize, Fut>
}

impl<Fut: Unpin> Unpin for SelectOk<Fut> {}

pub fn select_ok<F>(v: HashMap<usize, F>) -> SelectOk<F>
	where F: TryFuture + Unpin {

	SelectOk {
		inner: v
	}
}

impl<Fut: TryFuture + Unpin> Future for SelectOk<Fut> {
	type Output = Result<(Fut::Ok, HashMap<usize, Fut>), Fut::Error>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		loop {
			let item = self.inner.iter_mut().find_map(|(i, f)| {
				match (*f).try_poll_unpin(cx) {
					Poll::Pending => None,
					Poll::Ready(e) => Some((i, e)),
				}
			});
			match item {
				Some((idx, res)) => {
					let idx = *idx;
					self.inner.remove(&idx);
					match res {
						Ok(e) => {
							let inner = std::mem::replace(&mut self.inner, HashMap::new());
							return Poll::Ready(Ok((e, inner)))
						}
						Err(e) => {
							if self.inner.is_empty() {
								return Poll::Ready(Err(e))
							}
						}
					}
				}, None => {
					// based on the filter above, nothing is ready, return
					return Poll::Pending
				}
			}
		}
	}
}

// a generalized version of SelectOk above
pub struct SelectReady<'a, K, Fut> {
	inner: &'a mut HashMap<K, Fut>
}

impl<'a, K, Fut: Unpin> Unpin for SelectReady<'a, K, Fut> {}

pub fn select_ready<'a, F, K>(v: &'a mut HashMap<K, F>) -> SelectReady<'a, K, F>
	where K: Eq + std::hash::Hash, F: Future + Unpin {

	SelectReady {
		inner: v
	}
}

impl<'a, K: Clone + Eq + std::hash::Hash, Fut: Future + Unpin> Future for SelectReady<'a, K, Fut> {
	type Output = Option<(Fut::Output, K)>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		// we voluntarily keep returning Pending instead of Ready(None) to be able to select! on this
		//if self.inner.len() == 0 {
		//	return Poll::Ready(None);
		//}
		let item = self.inner.iter_mut().find_map(|(i, f)| {
			match (*f).poll_unpin(cx) {
				Poll::Pending => None,
				Poll::Ready(e) => Some((i, e)),
			}
		});
		match item {
			Some((idx, res)) => {
				let idx = idx.clone();
				self.inner.remove(&idx);
				return Poll::Ready(Some((res, idx)));
			}, None => {
				return Poll::Pending
			}
		}
	}
}

*/

pub struct PollWhileEnabled<O> {
	pub fut: Pin<Box<dyn Future<Output = O> + Send + 'static>>,
	pub enabled: Arc<AtomicBool>
}

impl<O> Future for PollWhileEnabled<O> {
	type Output = Option<O>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		if !self.enabled.load(Ordering::Acquire) {
			return Poll::Ready(None);
		}
		self.fut.as_mut().poll(cx).map(|e| Some(e))
	}
}
