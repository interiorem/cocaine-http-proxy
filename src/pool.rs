use std::boxed::FnBox;
use std::cmp;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::iter;
use std::time::{Duration, SystemTime};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::{Async, Future, Poll, Stream};
use futures::sync::mpsc;
use tokio_core::reactor::{Handle, Timeout};
use uuid::Uuid;

use cocaine::{Error, Service};
use cocaine::dispatch::Streaming;
use cocaine::logging::{Logger, Severity};
use cocaine::service::Locator;
use cocaine::service::locator::HashRing;

// TODO: Should not be public.
pub enum Event {
    Service {
        name: String,
        func: Box<FnBox(&Service) -> Box<Future<Item=(), Error=()> + Send> + Send>,
    },
    OnServiceConnect(Service),
    OnRoutingUpdates(HashMap<String, HashRing>),
}

struct WatchedService {
    service: Service,
    created_at: SystemTime,
}

impl WatchedService {
    fn new(service: Service, created_at: SystemTime) -> Self {
        Self {
            service: service,
            created_at: created_at,
        }
    }
}

struct ServicePool {
    log: Logger,

    /// Next service.
    counter: usize,
    name: String,
    limit: usize,
    /// Maximum service age.
    lifetime: Duration,
    handle: Handle,
    last_traverse: SystemTime,

    connecting: Arc<AtomicUsize>,
    connecting_limit: usize,
    services: VecDeque<WatchedService>,
    tx: mpsc::UnboundedSender<Event>,
}

impl ServicePool {
    fn new(name: String, limit: usize, handle: &Handle, tx: mpsc::UnboundedSender<Event>, log: Logger) -> Self {
        let now = SystemTime::now();

        Self {
            log: log,
            counter: 0,
            name: name.clone(),
            limit: limit,
            lifetime: Duration::new(5, 0),
            handle: handle.clone(),
            last_traverse: now,
            connecting: Arc::new(AtomicUsize::new(0)),
            connecting_limit: cmp::max(1, limit / 2),
            services: iter::repeat(name)
                .take(limit)
                .map(|name| WatchedService::new(Service::new(name.clone(), handle), now))
                .collect(),
            tx: tx
        }
    }

    fn push(&mut self, service: Service) {
        while self.services.len() >= self.limit {
            self.services.pop_front();
        }

        self.services.push_back(WatchedService::new(service, SystemTime::now()));
    }

    fn next(&mut self) -> &Service {
        let now = SystemTime::now();

        // No more than once every 5 seconds we're checking services for reconnection.
        if now.duration_since(self.last_traverse).unwrap() > Duration::new(5, 0) {
            self.last_traverse = now;

            cocaine_log!(self.log, Severity::Debug, "reconnecting at most {}/{} services",
                [self.connecting_limit - self.connecting.load(Ordering::Relaxed), self.services.len()]);

            while self.connecting.load(Ordering::Relaxed) < self.connecting_limit {
                match self.services.pop_front() {
                    Some(service) => {
                        if now.duration_since(service.created_at).unwrap() > self.lifetime {
                            cocaine_log!(self.log, Severity::Info, "reconnecting `{}` service", self.name);
                            self.connecting.fetch_add(1, Ordering::Relaxed);

                            let service = Service::new(self.name.clone(), &self.handle);
                            let future = service.connect();

                            let tx = self.tx.clone();
                            let log = self.log.clone();
                            self.handle.spawn(future.then(move |res| {
                                match res {
                                    Ok(()) => cocaine_log!(log, Severity::Info, "service `{}` has been reconnected", service.name()),
                                    Err(err) => {
                                        // Okay, we've tried our best. Insert the service anyway,
                                        // because internally it will try to establish connection
                                        // before each invocation attempt.
                                        cocaine_log!(log, Severity::Warn, "failed to reconnect `{}` service: {}", service.name(), err);
                                    }
                                }

                                tx.send(Event::OnServiceConnect(service)).unwrap();
                                Ok(())
                            }));
                        } else {
                            self.services.push_front(service);
                            break;
                        }
                    }
                    None => break,
                }
            }
        }

        self.counter = (self.counter + 1) % self.services.len();

        &self.services[self.counter].service
    }
}

enum RoutingState<T> {
    Fetch((String, T)),
    Retry(Timeout),
}

pub struct RoutingGroupsUpdateTask<T> {
    handle: Handle,
    locator: Locator,
    txs: Vec<mpsc::UnboundedSender<Event>>,
    log: Logger,
    /// Current state.
    state: Option<RoutingState<T>>,
    /// Number of unsuccessful routing table fetching attempts.
    attempts: u32,
    /// Maximum timeout value. Actual timeout is calculated using `1 << attempts` formula, but is
    /// truncated to this value if oversaturated.
    timeout_limit: Duration,
}

impl RoutingGroupsUpdateTask<Box<Stream<Item=Streaming<HashMap<String, HashRing>>, Error=Error>>> {
    pub fn new(handle: Handle, locator: Locator, txs: Vec<mpsc::UnboundedSender<Event>>, log: Logger) -> Self {
        let uuid = Uuid::new_v4().hyphenated().to_string();
        let stream = locator.routing(&uuid);

        Self {
            handle: handle,
            locator: locator,
            txs: txs,
            log: log,
            state: Some(RoutingState::Fetch((uuid, stream.boxed()))),
            attempts: 0,
            timeout_limit: Duration::new(32, 0),
        }
    }

    fn next_timeout(&self) -> Duration {
        // Hope that 2 ** 18 seconds, which is ~3 days, fits everyone needs.
        let exp = cmp::min(self.attempts, 18);
        let duration = Duration::new(2u64.pow(exp), 0);

        cmp::min(duration, self.timeout_limit)
    }
}

impl Future for RoutingGroupsUpdateTask<Box<Stream<Item=Streaming<HashMap<String, HashRing>>, Error=Error>>> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.state.take().expect("state must be valid") {
            RoutingState::Fetch((uuid, mut stream)) => {
                loop {
                    match stream.poll() {
                        Ok(Async::Ready(Some(Streaming::Write(groups)))) => {
                            self.attempts = 0;

                            for tx in &self.txs {
                                tx.send(Event::OnRoutingUpdates(groups.clone()))
                                    .expect("channel is bound to itself and lives forever");
                            }
                        }
                        Ok(Async::Ready(Some(Streaming::Error(err)))) => {
                            // TODO: Rework when futures 0.2 comes.
                            // Since at 0.1 streams are not terminated with an error we must inject
                            // our streaming protocol into a separate level. Squash it when futures
                            // 0.2 comes.
                            cocaine_log!(self.log, Severity::Warn, "failed to update RG: {}", [err], {
                                uuid: uuid,
                            });
                        }
                        Ok(Async::Ready(Some(Streaming::Close))) => {
                            cocaine_log!(self.log, Severity::Info, "locator has closed RG subscription", {
                                uuid: uuid,
                            });
                        }
                        Ok(Async::NotReady) => {
                            break;
                        }
                        Ok(Async::Ready(None)) | Err(..) => {
                            let timeout = self.next_timeout();
                            cocaine_log!(self.log, Severity::Debug, "next timeout will fire in {}s", [timeout.as_secs()]);

                            self.state = Some(RoutingState::Retry(Timeout::new(timeout, &self.handle)?));
                            self.attempts += 1;
                            return self.poll();
                        }
                    }
                }
                self.state = Some(RoutingState::Fetch((uuid, stream)));
            }
            RoutingState::Retry(mut timeout) => {
                match timeout.poll() {
                    Ok(Async::Ready(())) => {
                        cocaine_log!(self.log, Severity::Debug, "timed out, trying to subscribe routing ...");

                        let uuid = Uuid::new_v4().hyphenated().to_string();
                        let stream = self.locator.routing(&uuid);
                        self.state = Some(RoutingState::Fetch((uuid, stream.boxed())));
                        return self.poll();
                    }
                    Ok(Async::NotReady) => {
                        self.state = Some(RoutingState::Retry(timeout));
                    }
                    Err(..) => unreachable!(),
                }
            }
        }

        Ok(Async::NotReady)
    }
}

///
/// # Note
///
/// This task lives until all associated senders live:
/// - HTTP handlers.
/// - Timers.
/// - Unicorn notifiers.
/// - RG notifiers.
pub struct PoolTask {
    handle: Handle,
    log: Logger,

    tx: mpsc::UnboundedSender<Event>,
    rx: mpsc::UnboundedReceiver<Event>,

    pool: HashMap<String, ServicePool>,
}

impl PoolTask {
    pub fn new(handle: Handle, log: Logger, tx: mpsc::UnboundedSender<Event>, rx: mpsc::UnboundedReceiver<Event>) -> Self {
        Self {
            handle: handle,
            log: log,
            tx: tx,
            rx: rx,
            pool: HashMap::new(),
        }
    }

    fn select_service(&mut self, name: String, handle: &Handle) -> &Service {
        let tx = self.tx.clone();
        let log = self.log.clone();
        let mut pool = {
            let name = name.clone();
            self.pool.entry(name.clone())
                .or_insert_with(|| ServicePool::new(name, 10, handle, tx, log))
        };

        let now = SystemTime::now();
        while pool.services.len() + pool.connecting.load(Ordering::Relaxed) < 10 {
            pool.services.push_back(WatchedService::new(Service::new(name.clone(), handle), now))
        }

        pool.next()
    }
}

impl Future for PoolTask {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.rx.poll() {
                Ok(Async::Ready(Some(event))) => {
                    match event {
                        Event::Service { name, func } => {
                            // Select the next service that is not reconnecting right now. No more
                            // than N/2 services can be in reconnecting state concurrently.
                            let handle = self.handle.clone();
                            let ref service = self.select_service(name, &handle);

                            handle.spawn(func.call_box((service,)));
                        }
                        Event::OnServiceConnect(service) => {
                            match self.pool.get_mut(service.name()) {
                                Some(pool) => {
                                    pool.connecting.fetch_sub(1, Ordering::Relaxed);
                                    pool.push(service);
                                }
                                None => {
                                    println!("dropping service `{}` to unknown pool", service.name());
                                }
                            }
                        }
                        Event::OnRoutingUpdates(groups) => {
                            cocaine_log!(self.log, Severity::Info, "received {} RG(s) updates", groups.len());

                            for (group, ..) in groups {
                                if self.pool.contains_key(&group) {
                                    // TODO: For x in range(limit) connect -> then -> OnServiceConnect + rebalance.
                                }
                            }
                        }
                        // TODO: Unicorn timeout updates.
                        // TODO: Unicorn tracing chance updates.
                    }
                }
                Ok(Async::NotReady) => {
                    break;
                }
                Ok(Async::Ready(None)) | Err(..) => {
                    return Ok(Async::Ready(()));
                }
            }
        }

        Ok(Async::NotReady)
    }
}
