//! Run tests concurrently.
//!
//! This module provides the `ConcurrentRunner` struct which uses a pool of threads to run tests
//! concurrently.

use cretonne::timing;
use num_cpus;
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use {runone, TestResult};

/// Request sent to worker threads contains jobid and path.
struct Request(usize, PathBuf);

/// Reply from worker thread,
pub enum Reply {
    Starting { jobid: usize, thread_num: usize },
    Done { jobid: usize, result: TestResult },
    Tick,
}

/// Manage threads that run test jobs concurrently.
pub struct ConcurrentRunner {
    /// Channel for sending requests to the worker threads.
    /// The workers are sharing the receiver with an `Arc<Mutex<Receiver>>`.
    /// This is `None` when shutting down.
    request_tx: Option<Sender<Request>>,

    /// Channel for receiving replies from the workers.
    /// Workers have their own `Sender`.
    reply_rx: Receiver<Reply>,

    handles: Vec<thread::JoinHandle<timing::PassTimes>>,
}

impl ConcurrentRunner {
    /// Create a new `ConcurrentRunner` with threads spun up.
    pub fn new() -> Self {
        let (request_tx, request_rx) = channel();
        let request_mutex = Arc::new(Mutex::new(request_rx));
        let (reply_tx, reply_rx) = channel();

        heartbeat_thread(reply_tx.clone());

        let handles = (0..num_cpus::get())
            .map(|num| {
                worker_thread(num, request_mutex.clone(), reply_tx.clone())
            })
            .collect();

        Self {
            request_tx: Some(request_tx),
            reply_rx,
            handles,
        }
    }

    /// Shut down worker threads orderly. They will finish any queued jobs first.
    pub fn shutdown(&mut self) {
        self.request_tx = None;
    }

    /// Join all the worker threads.
    /// Transfer pass timings from the worker threads to the current thread.
    pub fn join(&mut self) {
        assert!(self.request_tx.is_none(), "must shutdown before join");
        for h in self.handles.drain(..) {
            match h.join() {
                Ok(t) => timing::add_to_current(&t),
                Err(e) => println!("worker panicked: {:?}", e),
            }
        }
    }

    /// Add a new job to the queues.
    pub fn put(&mut self, jobid: usize, path: &Path) {
        self.request_tx
            .as_ref()
            .expect("cannot push after shutdown")
            .send(Request(jobid, path.to_owned()))
            .expect("all the worker threads are gone");
    }

    /// Get a job reply without blocking.
    pub fn try_get(&mut self) -> Option<Reply> {
        self.reply_rx.try_recv().ok()
    }

    /// Get a job reply, blocking until one is available.
    pub fn get(&mut self) -> Option<Reply> {
        self.reply_rx.recv().ok()
    }
}

/// Spawn a heartbeat thread which sends ticks down the reply channel every second.
/// This lets us implement timeouts without the not yet stable `recv_timeout`.
fn heartbeat_thread(replies: Sender<Reply>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("heartbeat".to_string())
        .spawn(move || while replies.send(Reply::Tick).is_ok() {
            thread::sleep(Duration::from_secs(1));
        })
        .unwrap()
}

/// Spawn a worker thread running tests.
fn worker_thread(
    thread_num: usize,
    requests: Arc<Mutex<Receiver<Request>>>,
    replies: Sender<Reply>,
) -> thread::JoinHandle<timing::PassTimes> {
    thread::Builder::new()
        .name(format!("worker #{}", thread_num))
        .spawn(move || {
            loop {
                // Lock the mutex only long enough to extract a request.
                let Request(jobid, path) = match requests.lock().unwrap().recv() {
                    Err(..) => break, // TX end shut down. exit thread.
                    Ok(req) => req,
                };

                // Tell them we're starting this job.
                // The receiver should always be present for this as long as we have jobs.
                replies.send(Reply::Starting { jobid, thread_num }).unwrap();

                let result = catch_unwind(|| runone::run(path.as_path())).unwrap_or_else(|e| {
                    // The test panicked, leaving us a `Box<Any>`.
                    // Panics are usually strings.
                    if let Some(msg) = e.downcast_ref::<String>() {
                        Err(format!("panicked in worker #{}: {}", thread_num, msg))
                    } else if let Some(msg) = e.downcast_ref::<&'static str>() {
                        Err(format!("panicked in worker #{}: {}", thread_num, msg))
                    } else {
                        Err(format!("panicked in worker #{}", thread_num))
                    }
                });

                if let Err(ref msg) = result {
                    dbg!("FAIL: {}", msg);
                }

                replies.send(Reply::Done { jobid, result }).unwrap();
            }

            // Timing is accumulated independently per thread.
            // Timings from this worker thread will be aggregated by `ConcurrentRunner::join()`.
            timing::take_current()
        })
        .unwrap()
}
