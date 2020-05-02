use std::thread;
use std::collections::HashMap;

use crossbeam_channel::{Receiver, Sender, Select, bounded};

pub enum WorkerCommand<T> {
	Query(T),
	Exit
}

impl<T> WorkerCommand<T> {
	fn map<F, O>(self, fun: F) -> WorkerCommand<O> where F: Fn(T) -> O {
		if let WorkerCommand::Query(x) = self {
			WorkerCommand::Query(fun(x))
		} else {
			WorkerCommand::Exit
		}
	}
}

pub enum WorkerMessage<Q, V> {
	Done((Q, V)),
	Bye
}

pub trait TaskWorkerOperation<Q, A> {
	/// Return Some(x) to return this value to the sender, or None to stop this worker
	fn op(&mut self, query: Q) -> Option<A>;
}

pub struct TaskWorker<Q, A, WS> {
	state: WS,
	rx: Receiver<WorkerCommand<Q>>,
	tx: Sender<WorkerMessage<Q, A>>
}

impl<Q, A, WS> TaskWorker<Q, A, WS> where WS: TaskWorkerOperation<Q, A>, Q: Clone {
	fn worker_loop(&mut self) {
		loop {
			let query = self.rx.recv().unwrap();
			let val = match query {
				WorkerCommand::Query(q) => match self.state.op(q.clone()) {
					Some(x) => WorkerMessage::Done((q, x)),
					None => WorkerMessage::Bye
				}
				WorkerCommand::Exit => WorkerMessage::Bye
			};

			if let WorkerMessage::Bye = val {
				self.tx.send(val).unwrap();
				return;
			}
			self.tx.send(val).unwrap();
		}
	}

	fn run(mut self) {
		let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.worker_loop()));
		if let Err(_) = res {
			eprintln!("Worker crashed !");
			self.tx.send(WorkerMessage::Bye).unwrap();
		}
		res.unwrap();
	}
}

#[derive(Debug)]
pub enum ServerCommand<I: Clone, Q, WS> {
	Add((I, WS)),
	Query((I, Q)),
	Delete(I),
	Stop
}

#[derive(Debug, Clone)]
pub enum ServerCommandSuccess<I: Clone> {
	Add(I),
	Delete(I)
}

#[derive(Debug, Clone)]
pub enum ServerMessage<I: Clone> {
	Done(ServerCommandSuccess<I>),
	WrongIndex(I),
	Bye
}

pub struct TaskServer<I: Clone, Q: Clone, A, S, WS> {
	state: S,
	workers: HashMap<I, (Sender<WorkerCommand<Q>>, Receiver<WorkerMessage<Q, A>>)>,
	clients: Vec<(Receiver<ServerCommand<I, Q, WS>>, Sender<ServerMessage<I>>)>,
}

pub trait TaskServerOperation<I, Q, A> {
	fn op(&mut self, idx: &I, query: &Q, val: A);
	fn cleanup(&mut self, idx: &I);
}

impl<I, Q, A, S, WS> TaskServer<I, Q, A, S, WS>
	where S: TaskServerOperation<I, Q, A>,
	WS: Send + 'static + TaskWorkerOperation<Q, A>,
	I: std::hash::Hash + PartialEq + Eq + Send + Clone,
	Q: PartialEq + Eq + Send + Clone + 'static,
	A: Send + 'static {

	pub fn new(state: S) -> Self {
		TaskServer {
			state,
			workers: HashMap::new(),
			clients: Vec::new()
		}
	}

	pub fn gen_client(&mut self) -> (Sender<ServerCommand<I, Q, WS>>, Receiver<ServerMessage<I>>) {
		let (tx_name, rx_name) = bounded(25);
		let (tx_res, rx_res) = bounded(25);
		self.clients.push((rx_name, tx_res));
		(tx_name, rx_res)
	}

	fn spawn_worker(&self, workers_state: WS) -> (Sender<WorkerCommand<Q>>, Receiver<WorkerMessage<Q, A>>) {
		let (tx_query, rx_query) = bounded(10);
		let (tx_answer, rx_answer) = bounded(10);

		thread::spawn(move || {
			TaskWorker {
				state: workers_state,
				rx: rx_query,
				tx: tx_answer
			}.run()
		});

		(tx_query, rx_answer)
	}

	pub fn run(mut self) {
		if self.clients.len() == 0 {
			eprintln!("No clients registered for this server, not running anything");
			return;
		}
		let mut ongoing_operations = HashMap::new();
		let mut selector_map: HashMap<usize, I> = HashMap::new();
		loop {
			let mut sel = Select::new();
			for (rx, _tx) in &self.clients {
				sel.recv(&rx);
			}
			let mut sel_pos = self.clients.len();
			for (idx, (_, rx)) in &self.workers {
				sel.recv(&rx);
				selector_map.insert(sel_pos, idx.clone());
				sel_pos += 1;
			}
			let val = sel.select();
			let val_index = val.index();
			if val.index() < self.clients.len() {
				let (rx, tx) = &self.clients[val_index];
				let msg = val.recv(&rx).unwrap();
				match msg {
					ServerCommand::Add((idx, ws)) => {
						if self.workers.get(&idx).is_some() {
							// do nothing if the worker already exists, but log an error
							eprintln!("Task submitted twice !");
							continue;
						}
						let (worker_tx, worker_rx) = self.spawn_worker(ws);
						self.workers.insert(idx.clone(), (worker_tx, worker_rx));
						tx.send(ServerMessage::Done(ServerCommandSuccess::Add(idx))).unwrap();
					},
					ServerCommand::Query((idx, query)) => {
						if let Some((worker_tx, _)) = self.workers.get(&idx) {
							worker_tx.send(WorkerCommand::Query(query)).unwrap();
						} else {
							tx.send(ServerMessage::WrongIndex(idx)).unwrap();
						}
					},
					ServerCommand::Delete(idx) => {
						// ask the worker to exit, if it exists
						if let Some((worker_tx, _rx)) = self.workers.get(&idx) {
							worker_tx.send(WorkerCommand::Exit).unwrap();
							// link the operation to the client
							ongoing_operations.insert(idx, val_index);
						}
					},
					ServerCommand::Stop => {


					}
				}
			} else {
				let idx = (*selector_map.get(&val_index).unwrap()).clone();
				let (_, rx) = self.workers.get(&idx).unwrap();
				match val.recv(&rx).unwrap() {
					WorkerMessage::Done((k, v)) => {
						self.state.op(&idx, &k, v);
					},
					WorkerMessage::Bye => {
						selector_map.remove(&val_index);
						self.state.cleanup(&idx);
						let client_idx = ongoing_operations.remove(&idx).unwrap();
						self.clients[client_idx].1.send(ServerMessage::Done(ServerCommandSuccess::Delete(idx.clone()))).unwrap();

						// looper exited, let's remove it from the queue
						self.workers.remove(&idx);
					}
				}
			}
		}
	}
}
