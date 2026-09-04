//! The read pool (DESIGN §7.1).
//!
//! `AULOS_DB_READERS` threads, each holding one `SQLITE_OPEN_READ_ONLY` connection, behind a
//! [`tokio::sync::Semaphore`] of the same size. In WAL mode readers never block the writer and the
//! writer never blocks readers, so the pool exists to bound *memory and file descriptors*, not to
//! serialise anything.
//!
//! The semaphore is acquired **before** the job is queued, so at most `readers` jobs are ever in
//! the queue and an async caller waits on `.acquire()` instead of piling work onto a channel.

use std::sync::{Arc, Condvar, Mutex};

use rusqlite::Connection;

use crate::error::StoreError;
use crate::options::StoreOptions;
use crate::schema;

/// A queued read: it owns its own reply channel, so the pool needs no generics.
type ReadJob = Box<dyn FnOnce(&Connection) + Send + 'static>;

#[derive(Default)]
struct Queue {
    jobs: std::collections::VecDeque<ReadJob>,
    closed: bool,
}

/// `readers` threads and the semaphore that bounds what may be handed to them.
pub(crate) struct ReadPool {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    permits: Arc<tokio::sync::Semaphore>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl ReadPool {
    /// Opens `opts.effective_readers()` read-only connections and starts a thread for each.
    pub(crate) fn open(opts: &StoreOptions) -> Result<Self, StoreError> {
        let n = opts.effective_readers();
        let queue: Arc<(Mutex<Queue>, Condvar)> = Arc::default();
        let mut threads = Vec::with_capacity(n);
        for i in 0..n {
            let conn = schema::open_reader(opts)?;
            let queue = Arc::clone(&queue);
            let handle = std::thread::Builder::new()
                .name(format!("aulos-store-read-{i}"))
                .spawn(move || worker(&queue, &conn))
                .map_err(|e| StoreError::Io(format!("read pool thread: {e}").into_boxed_str()))?;
            threads.push(handle);
        }
        Ok(Self {
            queue,
            permits: Arc::new(tokio::sync::Semaphore::new(n)),
            threads: Mutex::new(threads),
        })
    }

    /// Runs `f` on a pool connection.
    pub(crate) async fn run<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| StoreError::Closed)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let (lock, cv) = &*self.queue;
            let mut q = lock.lock().map_err(|_| StoreError::Closed)?;
            if q.closed {
                return Err(StoreError::Closed);
            }
            q.jobs.push_back(Box::new(move |conn| {
                let out = f(conn);
                // The permit travels with the job so it is released when the query finishes, not
                // when it was queued.
                drop(permit);
                let _ = tx.send(out);
            }));
            cv.notify_one();
        }
        rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Stops the threads. Idempotent.
    pub(crate) fn close(&self) {
        let (lock, cv) = &*self.queue;
        if let Ok(mut q) = lock.lock() {
            q.closed = true;
            q.jobs.clear();
        }
        cv.notify_all();
        self.permits.close();
        if let Ok(mut threads) = self.threads.lock() {
            for t in threads.drain(..) {
                let _ = t.join();
            }
        }
    }
}

impl Drop for ReadPool {
    fn drop(&mut self) {
        self.close();
    }
}

/// One reader thread: take a job, run it, repeat.
fn worker(queue: &Arc<(Mutex<Queue>, Condvar)>, conn: &Connection) {
    let (lock, cv) = &**queue;
    loop {
        let job = {
            let Ok(mut q) = lock.lock() else { return };
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    break job;
                }
                if q.closed {
                    return;
                }
                match cv.wait(q) {
                    Ok(next) => q = next,
                    Err(_) => return,
                }
            }
        };
        job(conn);
    }
}
