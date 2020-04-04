use std::collections::HashMap;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc::{channel, Sender, Receiver};
use hyper::service::Service;
use futures::future::TryFuture;
use futures::future::TryFutureExt;

// a slightly modified version of the select_ok() method provided by futures_util
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
				}
				None => {
					// based on the filter above, nothing is ready, return
					return Poll::Pending
				}
			}
		}
	}
}


pub struct TaskClient<Q, A, E> {
	sender: Arc<Sender<Q>>,
	receiver: Arc<Receiver<A>>,
	_error: std::marker::PhantomData<E>
}

impl<Q, A, E> Clone for TaskClient<Q, A, E> {
	fn clone(self: &Self) -> Self {
		TaskClient {
			sender: self.sender.clone(),
			receiver: self.receiver.clone(),
			_error: std::marker::PhantomData
		}
	}
}

impl<Q, A, E> TaskClient<Q, A, E> {
	pub fn new(sender: Sender<Q>, receiver: Receiver<A>) -> Self {
		TaskClient {
			sender: Arc::new(sender),
			receiver: Arc::new(receiver),
			_error: std::marker::PhantomData
		}
	}
}

impl<Q, A, E> TaskClient<Q, A, E>
	where Q: Send + 'static,
	A: Send + 'static,
	E: std::fmt::Debug + From<tokio::sync::mpsc::error::SendError<Q>> {

	fn gen_future<R>(&mut self, query: Q, f: &'static (dyn Fn(Option<A>) -> R + Send + Sync + 'static)) -> Pin<Box<dyn Future<Output = R> + Send>> {
		// terrible hack due to the fact we cannot have a 'static lifetime here
		// we totally assume that the DnsResolverClient cannot disappear while the client is
		// operating
		let (sender, receiver) = unsafe {
			((Arc::get_mut_unchecked(&mut self.sender) as *mut Sender<Q>).as_mut().unwrap(),
			(Arc::get_mut_unchecked(&mut self.receiver) as *mut Receiver<A>).as_mut().unwrap())
		};

		Box::pin(async move {
			sender.send(query).await.map_err(|x| E::from(x)).unwrap();
			f(receiver.recv().await)
		})
	}
}

pub struct TaskClientService<Q, A: 'static, R: 'static, E: 'static> {
	task: TaskClient<Q, A, E>,
	fun: &'static (dyn Fn(Option<A>) -> Result<R, E> + Send + Sync + 'static)
}

impl<Q, A, R, E> Clone for TaskClientService<Q, A, R, E> {
	fn clone(self: &Self) -> Self {
		TaskClientService {
			task: self.task.clone(),
			fun: self.fun
		}
	}
}

impl<Q, A, R, E> Service<Q> for TaskClientService<Q, A, R, E>
	where Self: 'static,
	Q: Send,
	A: Send,
	E: std::fmt::Debug + From<tokio::sync::mpsc::error::SendError<Q>> {

	type Response = R;
	type Error = E;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
	}

	fn call(&mut self, query: Q) -> Self::Future {
		self.task.gen_future(query, self.fun)
	}
}

impl<Q, A, R, E> TaskClientService<Q, A, R, E> {
	pub fn new(client: TaskClient<Q, A, E>, fun: &'static (dyn Fn(Option<A>) -> Result<R, E> + Send + Sync + 'static)) -> Self {
		TaskClientService {
			task: client,
			fun
		}
	}
}

