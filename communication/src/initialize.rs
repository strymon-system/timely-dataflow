//! Initialization logic for a generic instance of the `Allocate` channel allocation trait.

use std::thread;
#[cfg(feature = "getopts")]
use std::io::BufRead;
#[cfg(feature = "getopts")]
use getopts;
use std::sync::Arc;

use std::any::Any;
use std::str::FromStr;

use crate::allocator::thread::ThreadBuilder;
use crate::allocator::{AllocateBuilder, Process, Generic, GenericBuilder};
use crate::allocator::zero_copy::initialize::initialize_networking;

use crate::logging::{CommunicationSetup, CommunicationEvent};
use logging_core::Logger;
use std::net::SocketAddrV4;
use crate::rescaling::bootstrap::{BootstrapSendEndpoint, BootstrapRecvEndpoint, bootstrap_worker_client};


/// Possible configurations for the communication infrastructure.
pub enum Configuration {
    /// Use one thread.
    Thread,
    /// Use one process with an indicated number of threads.
    Process(usize),
    /// Expect multiple processes.
    Cluster {
        /// Number of per-process worker threads
        threads: usize,
        /// Identity of this process
        process: usize,
        /// Addresses of all processes
        addresses: Vec<String>,
        /// Verbosely report connection process
        report: bool,
        /// Whether the current is joining the cluster
        join: Option<usize>,
        /// Closure to create a new logger for a communication thread
        log_fn: Box<Fn(CommunicationSetup) -> Option<Logger<CommunicationEvent, CommunicationSetup>> + Send + Sync>,
    }
}

#[cfg(feature = "getopts")]
impl Configuration {

    /// Returns a `getopts::Options` struct that can be used to print
    /// usage information in higher-level systems.
    pub fn options() -> getopts::Options {
        let mut opts = getopts::Options::new();
        opts.optopt("w", "threads", "number of per-process worker threads", "NUM");
        opts.optopt("p", "process", "identity of this process", "IDX");
        opts.optopt("n", "processes", "number of processes", "NUM");
        opts.optopt("h", "hostfile", "text file whose lines are process addresses", "FILE");
        opts.optopt("j", "join", "join the cluster with worker NUM as bootstrap server", "NUM");
        opts.optflag("r", "report", "reports connection progress");

        opts
    }

    /// Constructs a new configuration by parsing supplied text arguments.
    ///
    /// Most commonly, this uses `std::env::Args()` as the supplied iterator.
    pub fn from_args<I: Iterator<Item=String>>(args: I) -> Result<Configuration,String> {
        let opts = Configuration::options();

        opts.parse(args)
            .map_err(|e| format!("{:?}", e))
            .map(|matches| {

            // let mut config = Configuration::new(1, 0, Vec::new());
            let threads = matches.opt_str("w").map(|x| x.parse().unwrap_or(1)).unwrap_or(1);
            let process = matches.opt_str("p").map(|x| x.parse().unwrap_or(0)).unwrap_or(0);
            let processes = matches.opt_str("n").map(|x| x.parse().unwrap_or(1)).unwrap_or(1);
            let join = matches.opt_str("join").map(|x| x.parse::<usize>().unwrap());
            let report = matches.opt_present("report");

            assert!(process < processes);

            if processes > 1 {
                let mut addresses = Vec::new();
                if let Some(hosts) = matches.opt_str("h") {
                    let reader = ::std::io::BufReader::new(::std::fs::File::open(hosts.clone()).unwrap());
                    for x in reader.lines().take(processes) {
                        addresses.push(x.unwrap());
                    }
                    if addresses.len() < processes {
                        panic!("could only read {} addresses from {}, but -n: {}", addresses.len(), hosts, processes);
                    }
                }
                else {
                    for index in 0..processes {
                        addresses.push(format!("localhost:{}", 2101 + index));
                    }
                }

                assert!(processes == addresses.len());
                Configuration::Cluster {
                    threads,
                    process,
                    addresses,
                    report,
                    join,
                    log_fn: Box::new(|_| None),
                }
            }
            else if threads > 1 { Configuration::Process(threads) }
            else { Configuration::Thread }
        })
    }

    /// Attempts to assemble the described communication infrastructure.
    pub fn try_build(self) -> Result<(Vec<GenericBuilder>, (Option<Vec<BootstrapRecvEndpoint>>, Box<Any>)), String> {
        match self {
            Configuration::Thread => {
                Ok((vec![GenericBuilder::Thread(ThreadBuilder)], (None, Box::new(()))))
            },
            Configuration::Process(threads) => {
                Ok((Process::new_vector(threads).into_iter().map(|x| GenericBuilder::Process(x)).collect(), (None, Box::new(()))))
            },
            Configuration::Cluster { threads, process, addresses, report, join, log_fn } => {

                let (bootstrap_info, bootstrap_recv_endpoints) =
                    if let Some(server_index) = join {
                        let (sends, recvs) =
                            (0..threads).map(|_| {
                                let (state_tx, state_rx) = std::sync::mpsc::channel();
                                let (range_req_tx, range_req_rx) = std::sync::mpsc::channel();
                                let (range_ans_tx, range_ans_rx) = std::sync::mpsc::channel();

                                let send = BootstrapSendEndpoint::new(state_tx, range_req_rx, range_ans_tx);
                                let recv = BootstrapRecvEndpoint::new(state_rx, range_req_tx, range_ans_rx);
                                (send, recv)
                            }).unzip();

                        let bootstrap_address = std::env::var("BOOTSTRAP_ADDR").unwrap_or("localhost:9000".to_string());
                        let bootstrap_address = SocketAddrV4::from_str(bootstrap_address.as_str()).expect("cannot parse BOOTSTRAP_ADDRESS");

                        let bootstrap_info = Some((server_index, bootstrap_address));

                        // spawn the bootstrap thread, passing bootstrap endpoints (one for every worker thread to bootstrap)
                        std::thread::spawn(move || bootstrap_worker_client(bootstrap_address, sends));

                        (bootstrap_info, Some(recvs))
                    } else {
                        (None, None)
                    };


                match initialize_networking(addresses, process, threads, bootstrap_info, report, log_fn) {
                    Ok((stuff, guard)) => {
                        let builders = stuff.into_iter().map(|x| GenericBuilder::ZeroCopy(x)).collect();
                        Ok((builders, (bootstrap_recv_endpoints, Box::new(guard))))
                    },
                    Err(err) => Err(format!("failed to initialize networking: {}", err))
                }
            },
        }
    }
}

/// Initializes communication and executes a distributed computation.
///
/// This method allocates an `allocator::Generic` for each thread, spawns local worker threads,
/// and invokes the supplied function with the allocator.
/// The method returns a `WorkerGuards<T>` which can be `join`ed to retrieve the return values
/// (or errors) of the workers.
///
///
/// # Examples
/// ```
/// use timely_communication::Allocate;
///
/// use std::rc::Rc;
/// use std::cell::RefCell;
///
/// // configure for two threads, just one process.
/// let config = timely_communication::Configuration::Process(2);
///
/// // initializes communication, spawns workers
/// let guards = timely_communication::initialize(config, |mut allocator| {
///     println!("worker {} started", allocator.index());
///
///     // allocate senders and receiver
///     let senders1 = Rc::new(RefCell::new(Vec::new()));
///     let senders2 = Rc::clone(&senders1);
///
///     let on_new_pusher = move |pusher| {
///         senders1.borrow_mut().push(pusher);
///     };
///
///     // allocates pair of senders list and one receiver.
///     let mut receiver = allocator.allocate(0, on_new_pusher);
///
///     let mut senders = senders2.borrow_mut();
///
///     // send typed data along each channel
///     use timely_communication::Message;
///     senders[0].send(Message::from_typed(format!("hello, {}", 0)));
///     senders[1].send(Message::from_typed(format!("hello, {}", 1)));
///
///     // no support for termination notification,
///     // we have to count down ourselves.
///     let mut expecting = 2;
///     while expecting > 0 {
///         allocator.receive();
///         if let Some(message) = receiver.recv() {
///             use std::ops::Deref;
///             println!("worker {}: received: <{}>", allocator.index(), message.deref());
///             expecting -= 1;
///         }
///         allocator.release();
///     }
///
///     // optionally, return something
///     allocator.index()
/// });
///
/// // computation runs until guards are joined or dropped.
/// if let Ok(guards) = guards {
///     for guard in guards.join() {
///         println!("result: {:?}", guard);
///     }
/// }
/// else { println!("error in computation"); }
/// ```
///
/// The should produce output like:
///
/// ```ignore
/// worker 0 started
/// worker 1 started
/// worker 0: received: <hello, 0>
/// worker 1: received: <hello, 1>
/// worker 0: received: <hello, 0>
/// worker 1: received: <hello, 1>
/// result: Ok(0)
/// result: Ok(1)
/// ```
pub fn initialize<T:Send+'static, F: Fn(Generic)->T+Send+Sync+'static>(
    config: Configuration,
    func: F,
) -> Result<WorkerGuards<T>,String> {
    let (allocators, others) = config.try_build()?;
    assert!(others.0.is_none());
    let others = others.1;
    initialize_from(allocators, others, func)
}

/// Initializes computation and runs a distributed computation.
///
/// This version of `initialize` allows you to explicitly specify the allocators that
/// you want to use, by providing an explicit list of allocator builders. Additionally,
/// you provide `others`, a `Box<Any>` which will be held by the resulting worker guard
/// and dropped when it is dropped, which allows you to join communication threads.
///
/// # Examples
/// ```
/// use timely_communication::Allocate;
///
/// use std::rc::Rc;
/// use core::cell::RefCell;
///
/// // configure for two threads, just one process.
/// let builders = timely_communication::allocator::process::Process::new_vector(2);
///
/// // initializes communication, spawns workers
/// let guards = timely_communication::initialize_from(builders, Box::new(()), |mut allocator| {
///     println!("worker {} started", allocator.index());
///
///     // allocate senders and receiver
///     let senders1 = Rc::new(RefCell::new(Vec::new()));
///     let senders2 = Rc::clone(&senders1);
///
///     let on_new_pusher = move |pusher| {
///         senders1.borrow_mut().push(pusher);
///     };
///
///     // allocates pair of senders list and one receiver.
///     let mut receiver = allocator.allocate(0, on_new_pusher);
///     let mut senders = senders2.borrow_mut();
///
///     // send typed data along each channel
///     use timely_communication::Message;
///     senders[0].send(Message::from_typed(format!("hello, {}", 0)));
///     senders[1].send(Message::from_typed(format!("hello, {}", 1)));
///
///     // no support for termination notification,
///     // we have to count down ourselves.
///     let mut expecting = 2;
///     while expecting > 0 {
///         allocator.receive();
///         if let Some(message) = receiver.recv() {
///             use std::ops::Deref;
///             println!("worker {}: received: <{}>", allocator.index(), message.deref());
///             expecting -= 1;
///         }
///         allocator.release();
///     }
///
///     // optionally, return something
///     allocator.index()
/// });
///
/// // computation runs until guards are joined or dropped.
/// if let Ok(guards) = guards {
///     for guard in guards.join() {
///         println!("result: {:?}", guard);
///     }
/// }
/// else { println!("error in computation"); }
/// ```
pub fn initialize_from<A, T, F>(
    builders: Vec<A>,
    _others: Box<Any>,
    func: F,
) -> Result<WorkerGuards<T>,String>
where
    A: AllocateBuilder+'static,
    T: Send+'static,
    F: Fn(<A as AllocateBuilder>::Allocator)->T+Send+Sync+'static
{
    let logic = Arc::new(func);
    let mut guards = Vec::new();
    for (index, builder) in builders.into_iter().enumerate() {
        let clone = logic.clone();
        guards.push(thread::Builder::new()
                            .name(format!("worker thread {}", index))
                            .spawn(move || {
                                let communicator = builder.build();
                                (*clone)(communicator)
                            })
                            .map_err(|e| format!("{:?}", e))?);
    }

    Ok(WorkerGuards { guards, _others })
}

/// Maintains `JoinHandle`s for worker threads.
pub struct WorkerGuards<T:Send+'static> {
    guards: Vec<::std::thread::JoinHandle<T>>,
    _others: Box<Any>,
}

impl<T:Send+'static> WorkerGuards<T> {

    /// Returns a reference to the indexed guard.
    pub fn guards(&self) -> &[std::thread::JoinHandle<T>] {
        &self.guards[..]
    }

    /// Waits on the worker threads and returns the results they produce.
    pub fn join(mut self) -> Vec<Result<T, String>> {
        self.guards
            .drain(..)
            .map(|guard| guard.join().map_err(|e| format!("{:?}", e)))
            .collect()
    }
}

impl<T:Send+'static> Drop for WorkerGuards<T> {
    fn drop(&mut self) {
        for guard in self.guards.drain(..) {
            guard.join().expect("Worker panic");
        }
        // println!("WORKER THREADS JOINED");
    }
}
