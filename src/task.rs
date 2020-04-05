use std::collections::HashMap;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::future::Future;
use std::pin::Pin;
use std::marker::PhantomData;

use async_trait::async_trait;
use tokio::sync::mpsc::{channel, Sender, Receiver};
use tokio::sync::Mutex;
use hyper::service::Service;
use futures::future::TryFuture;
use futures::future::TryFutureExt;
use futures::future::FutureExt;

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

#[async_trait]
pub trait TaskServerOperation<Q, A> {
	async fn op(&self, query: Q) -> Option<A>;
}

pub struct TaskServer<Q, A, S> {
	state: S,
	// TODO: one mutex per client can lead to some significant memory overhead
	clients: Vec<Mutex<(Receiver<Q>, Sender<A>)>>
}

impl<Q, A, S> TaskServer<Q, A, S>
	where S: TaskServerOperation<Q, A>,
	Q: Send + 'static,
	A: std::fmt::Debug + Send + 'static {

	pub fn new(state: S) -> Self {
		TaskServer {
			state,
			clients: Vec::new()
		}
	}

	pub fn gen_client(&mut self) -> (Sender<Q>, Receiver<A>) {
		let (tx_name, rx_name) = channel(25);
		let (tx_res, rx_res) = channel(25);
		self.clients.push(Mutex::new((rx_name, tx_res)));
		(tx_name, rx_res)
	}

	fn gen_waiter<'a>(&'a self, i: usize) -> impl Future<Output = Result<(Q, usize), ()>> + 'a + Send {
		let client = self.clients[i].lock();
		async move {
			let mut client = client.await;
			match client.0.recv().await {
				Some(val) => Ok((val, i)),
				None => Err(())
			}
		}
	}

	fn gen_waiters<'a>(&'a self) -> HashMap<usize, Pin<Box<dyn Future<Output = Result<(Q, usize), ()>> + Send + 'a>>> {
		// all thoses allocations probably have a big overhead but are only
		// executed once per load
		let mut v: HashMap<usize, Pin<Box<dyn Future<Output = Result<(Q, usize), ()>> + Send>>> = HashMap::with_capacity(self.clients.len());
		for i in 0..self.clients.len() {
			v.insert(i, Box::pin(self.gen_waiter(i)));
		}
		v
	}

	pub async fn run(self) {
		if self.clients.len() == 0 {
			eprintln!("The service was started with 0 clients, returning immediately");
			return;
		}

		let waiters = self.gen_waiters();

		let mut awaiting_future = select_ok(waiters);

		loop {
			match awaiting_future.await {
				Ok(((msg, idx), mut rest)) => {
					// re-add a waiter for this element
					rest.insert(idx, Box::pin(self.gen_waiter(idx)));
					awaiting_future = select_ok(rest);

					if let Some(res) = self.state.op(msg).await {
						let mut client = self.clients.get(idx).unwrap().lock().await;
						client.1.send(res).await.unwrap();
					}
				}, Err(()) => {
					// return when all channels are closed
					return;
				}
			}
		}
	}
}

pub struct TaskClient<Q, A, E> {
	sender: Arc<Sender<Q>>,
	receiver: Arc<Receiver<A>>,
	_error: PhantomData<E>
}

impl<Q, A, E> Clone for TaskClient<Q, A, E> {
	fn clone(self: &Self) -> Self {
		TaskClient {
			sender: self.sender.clone(),
			receiver: self.receiver.clone(),
			_error: PhantomData
		}
	}
}

impl<Q, A, E> TaskClient<Q, A, E> {
	pub fn new(sender: Sender<Q>, receiver: Receiver<A>) -> Self {
		TaskClient {
			sender: Arc::new(sender),
			receiver: Arc::new(receiver),
			_error: PhantomData
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

