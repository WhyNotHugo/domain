//! A DNS Resolver using rotor.

mod conn;
mod dispatcher;
mod query;
mod stream;
mod sync;
mod tcp;
mod timeout;
mod udp;

use std::io;
use std::sync::mpsc::TryRecvError;
use std::thread;
use rotor::{self, EventSet, GenericScope, Machine, Notifier, Response,
            Scope, Void};
use bits::message::MessageBuf;
use resolv::conf::ResolvConf;
use resolv::error::{Error, Result};
use resolv::tasks::{Progress, Task};
use self::dispatcher::{BootstrapItem, Dispatcher};
use self::query::Query;
use self::sync::{RotorReceiver, RotorSender};
use self::tcp::TcpTransport;
use self::udp::UdpTransport;


//------------ DnsTransport -------------------------------------------------

/// The rotor state machine for the DNS transport.
pub struct DnsTransport<X>(Composition<X>);

impl<X> DnsTransport<X> {
    /// Creates a new DNS transport.
    ///
    /// Returns the transport and a resolver.
    pub fn new<S: GenericScope>(conf: ResolvConf, scope: &mut S)
                                -> (Self, Resolver) {
        let (dispatcher, tx) = Dispatcher::new(conf, scope);
        let resolver = Resolver::new(tx);
        (DnsTransport(Composition::Dispatcher(dispatcher)),
         resolver)
    }

    /// Spawns a new DNS transport in a new thread.
    ///
    /// Returns the `JoinHandle` for this new thread and a resolver.
    pub fn spawn(conf: ResolvConf)
                 -> io::Result<(thread::JoinHandle<()>, Resolver)> {
        let mut loop_creator = try!(rotor::Loop::new(&rotor::Config::new()));
        let mut res = None;
        loop_creator.add_machine_with(|scope| {
            let (transport, resolver) = DnsTransport::new(conf, scope);
            res = Some(resolver);
            Response::ok(transport)
        }).unwrap(); // Only NoSlabSpace can happen which is fatal ...
        let child = thread::spawn(move || {
            loop_creator.run(()).ok();
        });
        Ok((child, res.unwrap()))
    }
}


impl<X> Machine for DnsTransport<X> {
    type Context = X;
    type Seed = BootstrapItem;

    fn create(seed: Self::Seed, scope: &mut Scope<Self::Context>)
              -> Response<Self, Void> {
        use self::dispatcher::BootstrapItem::*;

        match seed {
            Udp(s) => UdpTransport::create(s, scope)
                                   .map(|m| DnsTransport(Composition::Udp(m)),
                                        |_| unreachable!()),
            Tcp(s) => TcpTransport::create(s, scope)
                                   .map(|m| DnsTransport(Composition::Tcp(m)),
                                        |_| unreachable!()),
        }
    }

    fn ready(self, events: EventSet, scope: &mut Scope<Self::Context>)
             -> Response<Self, Self::Seed> {
        use self::Composition::*;

        match self.0 {
            Dispatcher(m) => m.ready(events, scope)
                              .map(|m| DnsTransport(Dispatcher(m)), |x| x),
            Udp(m) => m.ready(events, scope)
                       .map(|m| DnsTransport(Udp(m)), |_| unreachable!()),
            Tcp(m) => m.ready(events, scope)
                       .map(|m| DnsTransport(Tcp(m)), |_| unreachable!()),
        }
    }

    fn spawned(self, scope: &mut Scope<Self::Context>)
               -> Response<Self, Self::Seed> {
        use self::Composition::*;

        match self.0 {
            Dispatcher(m) => m.spawned(scope)
                              .map(|m| DnsTransport(Dispatcher(m)), |x| x),
            Udp(m) => m.spawned(scope)
                       .map(|m| DnsTransport(Udp(m)), |_| unreachable!()),
            Tcp(m) => m.spawned(scope)
                       .map(|m| DnsTransport(Tcp(m)), |_| unreachable!()),
        }
    }

    fn timeout(self, scope: &mut Scope<Self::Context>)
               -> Response<Self, Self::Seed> {
        use self::Composition::*;

        match self.0 {
            Dispatcher(m) => m.timeout(scope)
                              .map(|m| DnsTransport(Dispatcher(m)), |x| x),
            Udp(m) => m.timeout(scope)
                       .map(|m| DnsTransport(Udp(m)), |_| unreachable!()),
            Tcp(m) => m.timeout(scope)
                       .map(|m| DnsTransport(Tcp(m)), |_| unreachable!()),
        }
    }

    fn wakeup(self, scope: &mut Scope<Self::Context>)
              -> Response<Self, Self::Seed> {
        use self::Composition::*;

        match self.0 {
            Dispatcher(m) => m.wakeup(scope)
                              .map(|m| DnsTransport(Dispatcher(m)), |x| x),
            Udp(m) => m.wakeup(scope)
                       .map(|m| DnsTransport(Udp(m)), |_| unreachable!()),
            Tcp(m) => m.wakeup(scope)
                       .map(|m| DnsTransport(Tcp(m)), |_| unreachable!()),
        }
    }
}


//------------ Composition --------------------------------------------------

/// The composition of all our rotor state machines.
///
/// This is only for hiding internals.
enum Composition<X> {
    Dispatcher(Dispatcher<X>),
    Udp(UdpTransport<X>),
    Tcp(TcpTransport<X>),
}


//------------ Resolver -----------------------------------------------------

/// The resolver.
#[derive(Clone)]
pub struct Resolver {
    requests: RotorSender<Query>,
}

impl Resolver {
    fn new(requests: RotorSender<Query>) -> Self {
        Resolver { requests: requests }
    }

    /// Processes a task synchronously, ie., waits for an answer.
    pub fn sync_task<T: Task>(&self, task: T) -> Result<T::Success> {
        let mut machine = try!(ResolverMachine::new(&self, task, None));
        loop {
            match machine.step() {
                Progress::Continue(m) => machine = m,
                Progress::Success(s) => return Ok(s),
                Progress::Error(e) => return Err(e)
            }
        }
    }

    /// Processes a task asynchronously by returning a machine.
    pub fn task<T: Task, X>(&self, task: T, scope: &mut Scope<X>)
                            -> Result<ResolverMachine<T>> {
        ResolverMachine::new(self, task, Some(scope.notifier()))
    }
}


//------------ ResolverMachine ----------------------------------------------

pub struct ResolverMachine<T: Task> {
    requests: RotorSender<Query>,
    receiver: RotorReceiver<Result<MessageBuf>>,
    task: T,
}

impl<T: Task> ResolverMachine<T> {
    fn new(resolver: &Resolver, mut task: T, notifier: Option<Notifier>)
           -> Result<Self> {
        let requests = resolver.requests.clone();
        let receiver = RotorReceiver::new(notifier);
        let mut res = Ok(());
        task = task.start(|qname, qtype, qclass| {
            let message = match MessageBuf::query_from_question(
                                                    &(qname, qtype, qclass)) {
                Ok(message) => message,
                Err(err) => { res = Err(err); return }
            };
            let query = Query::new(message, receiver.sender());
            requests.send(query).unwrap(); // XXX Handle error.
        });
        if let Err(err) = res {
            return Err(err.into());
        }
        Ok(ResolverMachine { requests: requests, receiver: receiver,
                             task: task })
    }

    pub fn wakeup(self) -> Progress<Self, T::Success> {
        let response = match self.receiver.try_recv() {
            Ok(response) => response,
            Err(TryRecvError::Empty) => return Progress::Continue(self),
            Err(TryRecvError::Disconnected) => {
                return Progress::Error(Error::Timeout) // XXX Hmm.
            }
        };
        self.progress(response)
    }

    pub fn step(self) -> Progress<Self, T::Success> {
        let response = match self.receiver.recv() {
            Ok(response) => response,
            Err(..) => return Progress::Error(Error::Timeout),
        };
        self.progress(response)
    }

    fn progress(self, response: Result<MessageBuf>)
                -> Progress<Self, T::Success> {
        let (task, receiver, requests) = (self.task, self.receiver,
                                          self.requests);
        let mut res = Ok(());
        let progress = task.progress(response, |qname, qtype, qclass| {
            let message = match MessageBuf::query_from_question(
                                                   &(qname, qtype, qclass)) {
                Ok(message) => message,
                Err(err) => { res = Err(err); return }
            };
            let query = Query::new(message, receiver.sender());
            requests.send(query).unwrap(); // XXX Handle error.
        });
        if let Err(err) = res {
            return Progress::Error(err.into())
        }
        match progress {
            Progress::Continue(t) => {
                Progress::Continue(ResolverMachine { receiver: receiver,
                                                     requests: requests,
                                                     task: t })
            }
            Progress::Success(s) => Progress::Success(s),
            Progress::Error(e) => Progress::Error(e),
        }
    }
}

