#![forbid(missing_docs)]
// License at https://github.com/NicoElbers/easy_threadpool
// It's MIT

//! A simple thread pool to execute jobs in parallel
//!
//! A simple crate without dependencies which allows you to create a threadpool
//! that has a specified amount of threads which execute given jobs. Threads don't
//! crash when a job panics!
//!
//! # Examples
//!
//! ## Basic usage
//!
//! A basic use of the threadpool
//!
//! ```rust
//! # use std::error::Error;
//! # fn main() -> Result<(), Box<dyn Error>> {
//! use easy_threadpool::ThreadPoolBuilder;
//!
//! fn job() {
//!     println!("Hello world!");
//! }
//!
//! let builder = ThreadPoolBuilder::with_max_threads()?;
//! let pool = builder.build()?;
//!
//! for _ in 0..10 {
//!     pool.send_job(job);
//! }
//!
//! assert!(pool.wait_until_finished().is_ok());
//! # Ok(())
//! # }
//! ```
//!
//! ## More advanced usage
//!
//! A slightly more advanced usage of the threadpool
//!
//! ```rust
//! # use std::error::Error;
//! # fn main() -> Result<(), Box<dyn Error>> {
//! use easy_threadpool::ThreadPoolBuilder;
//! use std::sync::mpsc::channel;
//!
//! let builder = ThreadPoolBuilder::with_max_threads()?;
//! let pool = builder.build()?;
//!
//! let (tx, rx) = channel();
//!
//! for _ in 0..10 {
//!     let tx = tx.clone();
//!     pool.send_job(move || {
//!         tx.send(1).expect("Receiver should still exist");
//!     });
//! }
//!
//! assert!(pool.wait_until_finished().is_ok());
//!
//! assert_eq!(rx.iter().take(10).fold(0, |a, b| a + b), 10);
//! # Ok(())
//! # }
//! ```
//!
//! ## Dealing with panics
//!
//! This threadpool implementation is resistant to jobs panicing
//!
//! ```rust
//! # use std::error::Error;
//! # fn main() -> Result<(), Box<dyn Error>> {
//! use easy_threadpool::ThreadPoolBuilder;
//! use std::sync::mpsc::channel;
//! use std::num::NonZeroUsize;
//!
//! fn panic_fn() {
//!     panic!("Test panic");
//! }
//!
//! let num = NonZeroUsize::try_from(1)?;
//! let builder = ThreadPoolBuilder::with_thread_amount(num);
//! let pool = builder.build()?;
//!
//! let (tx, rx) = channel();
//! for _ in 0..10 {
//!     let tx = tx.clone();
//!     pool.send_job(move || {
//!         tx.send(1).expect("Receiver should still exist");
//!         panic!("Test panic");
//!     });
//! }
//!
//! assert!(pool.wait_until_finished().is_err());
//! pool.wait_until_finished_unchecked();
//!
//! assert_eq!(pool.jobs_paniced(), 10);
//! assert_eq!(rx.iter().take(10).fold(0, |a, b| a + b), 10);
//! # Ok(())
//! # }
//! ```

mod job;
mod shared_state;

use std::{
    error::Error,
    fmt::{Debug, Display},
    io,
    num::{NonZeroUsize, TryFromIntError},
    panic::{catch_unwind, UnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{channel, Sender},
        Arc, Condvar, Mutex,
    },
    thread::{self, available_parallelism},
};

type ThreadPoolFunctionBoxed = Box<dyn FnOnce() + Send + UnwindSafe>;

/// Simple error to indicate that a job has paniced in the threadpool
#[derive(Debug)]
pub struct JobHasPanicedError {}

impl Display for JobHasPanicedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "At least one job in the threadpool has caused a panic")
    }
}

impl Error for JobHasPanicedError {}

// /// Simple error to indicate a function passed to do_until_finished has paniced
// #[derive(Debug)]
// pub struct DoUntilFinishedFunctionPanicedError {}

// impl Display for DoUntilFinishedFunctionPanicedError {
//     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//         write!(f, "The function passed to do_until_finished has paniced")
//     }
// }

// impl Error for DoUntilFinishedFunctionPanicedError {}

// /// An enum to combine both errors previously defined
// #[derive(Debug)]
// pub enum Errors {
//     /// Enum representation of [`JobHasPanicedError`]
//     JobHasPanicedError,
//     /// Enum representation of [`DoUntilFinishedFunctionPanicedError`]
//     DoUntilFinishedFunctionPanicedError,
// }

// impl Display for Errors {
//     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//         match self {
//             Errors::DoUntilFinishedFunctionPanicedError => {
//                 Display::fmt(&DoUntilFinishedFunctionPanicedError {}, f)
//             }
//             Errors::JobHasPanicedError => Display::fmt(&JobHasPanicedError {}, f),
//         }
//     }
// }

// impl Error for Errors {}

#[derive(Debug, Default)]
struct SharedState {
    jobs_queued: AtomicUsize,
    jobs_running: AtomicUsize,
    jobs_paniced: AtomicUsize,
    is_finished: Mutex<bool>,
    has_paniced: AtomicBool,
}

impl Display for SharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SharedState<jobs_queued: {}, jobs_running: {}, jobs_paniced: {}, is_finished: {}, has_paniced: {}>",
            self.jobs_queued.load(Ordering::Relaxed),
            self.jobs_running.load(Ordering::Relaxed),
            self.jobs_paniced.load(Ordering::Relaxed),
            self.is_finished.lock().expect("Shared state should never panic"),
            self.has_paniced.load(Ordering::Relaxed)
        )
    }
}

impl SharedState {
    fn new() -> Self {
        Self {
            jobs_running: AtomicUsize::new(0),
            jobs_queued: AtomicUsize::new(0),
            jobs_paniced: AtomicUsize::new(0),
            is_finished: Mutex::new(true),
            has_paniced: AtomicBool::new(false),
        }
    }

    fn job_starting(&self) {
        debug_assert!(
            self.jobs_queued.load(Ordering::Acquire) > 0,
            "Negative jobs queued"
        );

        self.jobs_running.fetch_add(1, Ordering::SeqCst);
        self.jobs_queued.fetch_sub(1, Ordering::SeqCst);
    }

    fn job_finished(&self) {
        debug_assert!(
            self.jobs_running.load(Ordering::Acquire) > 0,
            "Negative jobs running"
        );

        self.jobs_running.fetch_sub(1, Ordering::SeqCst);

        if self.jobs_queued.load(Ordering::Acquire) == 0
            && self.jobs_running.load(Ordering::Acquire) == 0
        {
            let mut is_finished = self
                .is_finished
                .lock()
                .expect("Shared state should never panic");

            *is_finished = true;
        }
    }

    fn job_queued(&self) {
        self.jobs_queued.fetch_add(1, Ordering::SeqCst);

        let mut is_finished = self
            .is_finished
            .lock()
            .expect("Shared state should never panic");

        *is_finished = false;
    }

    fn job_paniced(&self) {
        println!("Checking panic");

        self.has_paniced.store(true, Ordering::SeqCst);
        self.jobs_paniced.fetch_add(1, Ordering::SeqCst);

        println!("Has paniced {}", self.has_paniced.load(Ordering::Acquire));
    }
}

/// Threadpool abstraction to keep some state
#[derive(Debug)]
pub struct ThreadPool {
    thread_amount: NonZeroUsize,
    job_sender: Arc<Sender<ThreadPoolFunctionBoxed>>,
    shared_state: Arc<SharedState>,
    cvar: Arc<Condvar>,
}

impl Clone for ThreadPool {
    fn clone(&self) -> Self {
        Self {
            thread_amount: self.thread_amount,
            job_sender: self.job_sender.clone(),
            shared_state: self.shared_state.clone(),
            cvar: self.cvar.clone(),
        }
    }
}

impl Display for ThreadPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Threadpool< thread_amount: {}, shared_state: {}>",
            self.thread_amount, self.shared_state
        )
    }
}

impl ThreadPool {
    fn new(builder: ThreadPoolBuilder) -> io::Result<Self> {
        let thread_amount = builder.thread_amount;

        let (job_sender, job_receiver) = channel::<ThreadPoolFunctionBoxed>();
        let job_sender = Arc::new(job_sender);
        let shareable_job_reciever = Arc::new(Mutex::new(job_receiver));

        let shared_state = Arc::new(SharedState::new());
        let cvar = Arc::new(Condvar::new());

        for thread_num in 0..thread_amount.get() {
            let job_reciever = shareable_job_reciever.clone();

            let thread_name = format!("Threadpool worker {thread_num}");

            thread::Builder::new().name(thread_name).spawn(move || {
                loop {
                    let job = {
                        let lock = job_reciever //
                            .lock()
                            .expect("Cannot get reciever");

                        lock.recv()
                    };

                    // NOTE: Breaking on error ensures that all threads will stop
                    // when the threadpool is dropped and all jobs have been executed
                    match job {
                        Ok(job) => job(),
                        Err(_) => break,
                    };
                }
            })?;
        }

        Ok(Self {
            thread_amount,
            job_sender,
            shared_state,
            cvar,
        })
    }

    /// The `send_job` function takes in a function or closure without any arguments
    /// and sends it to the threadpool to be executed. Jobs will be taken from the
    /// job queue in order of them being sent, but that in no way guarantees they will
    /// be executed in order.
    ///
    /// `job`s must implement `Send` in order to be safely sent across threads and
    /// `UnwindSafe` to allow catching panics when executing the jobs. Both of these
    /// traits are auto implemented.
    ///
    /// # Examples
    ///
    /// Sending a function or closure to the threadpool
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// fn job() {
    ///     println!("Hello world from a function!");
    /// }
    ///
    /// let builder = ThreadPoolBuilder::with_max_threads()?;
    /// let pool = builder.build()?;
    ///
    /// pool.send_job(job);
    ///
    /// pool.send_job(|| println!("Hello world from a closure!"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn send_job(&self, job: impl FnOnce() + Send + UnwindSafe + 'static) {
        // NOTE: It is essential that the shared state is updated FIRST otherwise
        // we have a race condidition that the job is transmitted and read before
        // the shared state is updated, leading to a negative amount of jobs queued
        self.shared_state.job_queued();

        debug_assert!(self.jobs_queued() > 0, "Job didn't queue properly");
        debug_assert!(!self.is_finished(), "Finish wasn't properly set to false");

        // Pass our own state to the job. This makes it so that multiple threadpools
        // with different states can send jobs to the same threads without getting
        // eachothers panics for example
        let state = self.shared_state.clone();
        let cvar = self.cvar.clone();
        let job_with_state = Self::job_function(Box::new(job), state, cvar);

        self.job_sender
            .send(Box::new(job_with_state))
            .expect("The sender cannot be deallocated while the threadpool is in use")
    }

    fn job_function(
        job: ThreadPoolFunctionBoxed,
        state: Arc<SharedState>,
        cvar: Arc<Condvar>,
    ) -> impl FnOnce() + Send + 'static {
        move || {
            state.job_starting();

            // NOTE: The use of catch_unwind means that the thread will not
            // panic from any of the jobs it was sent. This is useful because
            // we won't ever have to restart a thread.
            let result = catch_unwind(job);

            println!("{result:?}");

            // NOTE: Do the panic check first otherwise we have a race condition
            // where the final job panics and the wait_until_finished function
            // doesn't detect it
            if result.is_err() {
                state.job_paniced();
            }

            state.job_finished();

            cvar.notify_all();
        }
    }

    /// This function will wait until all jobs have finished sending. Additionally
    /// it will return early if any job panics.
    ///
    /// Be careful though, returning early DOES NOT mean that the sent jobs are
    /// cancelled. They will remain running. Cancelling jobs that are queued is not
    /// a feature provided by this crate as of now.
    ///
    /// # Errors
    ///
    /// This function will error if any job sent to the threadpool has errored.
    /// This includes any errors since either the threadpool was created or since
    /// the state was reset.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// let builder = ThreadPoolBuilder::with_max_threads()?;
    /// let pool = builder.build()?;
    ///
    /// for _ in 0..10 {
    ///     pool.send_job(|| println!("Hello world"));
    /// }
    ///
    /// assert!(pool.wait_until_finished().is_ok());
    /// assert!(pool.is_finished());
    ///
    /// pool.send_job(|| panic!("Test panic"));
    ///
    /// assert!(pool.wait_until_finished().is_err());
    /// assert!(pool.has_paniced());
    /// # Ok(())
    /// # }
    /// ```
    pub fn wait_until_finished(&self) -> Result<(), JobHasPanicedError> {
        let mut is_finished = self
            .shared_state
            .is_finished
            .lock()
            .expect("Shared state should never panic");

        while !*is_finished && !self.has_paniced() {
            is_finished = self
                .cvar
                .wait(is_finished)
                .expect("Shared state should never panic");
        }

        println!("panic {}", self.has_paniced());

        debug_assert!(
            self.has_paniced() || self.jobs_running() == 0,
            "wait_until_finished stopped {} jobs running and {} panics",
            self.jobs_running(),
            self.jobs_paniced()
        );
        debug_assert!(
            self.has_paniced() || self.jobs_queued() == 0,
            "wait_until_finished stopped while {} jobs queued and {} panics",
            self.jobs_queued(),
            self.jobs_paniced()
        );

        println!("WERE DONE WAITING");

        match self.shared_state.has_paniced.load(Ordering::Acquire) {
            true => Err(JobHasPanicedError {}),
            false => Ok(()),
        }
    }

    /// This function will wait until one job finished after calling the function.
    /// Additionally, if the threadpool is finished this function will also return.
    /// Additionally it will return early if any job panics.
    ///
    /// Be careful though, returning early DOES NOT mean that the sent jobs are
    /// cancelled. They will remain running. Cancelling jobs that are queued is not
    /// a feature provided by this crate as of now.
    ///
    /// # Errors
    ///
    /// This function will error if any job sent to the threadpool has errored.
    /// This includes any errors since either the threadpool was created or since
    /// the state was reset.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// let builder = ThreadPoolBuilder::with_max_threads()?;
    /// let pool = builder.build()?;
    ///
    /// assert!(pool.wait_until_job_done().is_ok());
    /// assert!(pool.is_finished());
    ///
    /// pool.send_job(|| panic!("Test panic"));
    ///
    /// assert!(pool.wait_until_job_done().is_err());
    /// assert!(pool.has_paniced());
    /// # Ok(())
    /// # }
    /// ```
    pub fn wait_until_job_done(&self) -> Result<(), JobHasPanicedError> {
        fn paniced(state: &SharedState) -> bool {
            state.jobs_paniced.load(Ordering::Acquire) != 0
        }

        let is_finished = self
            .shared_state
            .is_finished
            .lock()
            .expect("Shared state should never panic");

        if *is_finished {
            return Ok(());
        };

        drop(self.cvar.wait(is_finished));

        // Keep the guard so we don't have to drop the lock only to reaquire it
        if paniced(&self.shared_state) {
            Err(JobHasPanicedError {})
        } else {
            Ok(())
        }
    }

    /// This function will wait until all jobs have finished sending. It will continue
    /// waiting if a job panics in the thread pool.
    ///
    /// I highly doubt this has much of a performance improvement, but it's very
    /// useful if you know that for whatever reason your jobs might panic and that
    /// would be fine.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// let builder = ThreadPoolBuilder::with_max_threads()?;
    /// let pool = builder.build()?;
    ///
    /// for _ in 0..10 {
    ///     pool.send_job(|| println!("Hello world"));
    /// }
    ///
    /// pool.wait_until_finished_unchecked();
    /// assert!(pool.is_finished());
    ///
    /// pool.send_job(|| panic!("Test panic"));
    ///
    /// pool.wait_until_finished_unchecked();
    /// assert!(pool.has_paniced());
    /// # Ok(())
    /// # }
    /// ```
    pub fn wait_until_finished_unchecked(&self) {
        let mut is_finished = self
            .shared_state
            .is_finished
            .lock()
            .expect("Shared state sould never panic");

        if *is_finished {
            return;
        }

        while !*is_finished {
            is_finished = self
                .cvar
                .wait(is_finished)
                .expect("Shared state should never panic")
        }

        debug_assert!(
            self.shared_state.jobs_running.load(Ordering::Acquire) == 0,
            "Job still running after wait_until_finished_unchecked"
        );
        debug_assert!(
            self.shared_state.jobs_queued.load(Ordering::Acquire) == 0,
            "Job still queued after wait_until_finished_unchecked"
        );
    }

    /// This function will wait until one job finished after calling the function.
    /// Additionally, if the threadpool is finished this function will also return.
    ///
    /// Be careful though, returning early DOES NOT mean that the sent jobs are
    /// cancelled. They will remain running. Cancelling jobs that are queued is not
    /// a feature provided by this crate as of now.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// let builder = ThreadPoolBuilder::with_max_threads()?;
    /// let pool = builder.build()?;
    ///
    /// assert!(pool.wait_until_job_done().is_ok());
    /// assert!(pool.is_finished());
    ///
    /// pool.send_job(|| panic!("Test panic"));
    ///
    /// assert!(pool.wait_until_job_done().is_err());
    /// assert!(pool.has_paniced());
    /// # Ok(())
    /// # }
    /// ```
    pub fn wait_until_job_done_unchecked(&self) {
        let is_finished = self
            .shared_state
            .is_finished
            .lock()
            .expect("Shared state should never panic");

        // This is guaranteed to work because jobs cannot finish without having
        // the shared state lock, and we keep the lock until we start waiting for
        // the condvar
        if *is_finished {
            return;
        };

        drop(self.cvar.wait(is_finished));
    }

    /// This function will reset the state of this instance of the threadpool.
    ///
    /// When resetting the state you lose all information about previously sent jobs.
    /// If a job you previously sent panics, you will not be notified, nor can  you
    /// wait until your previously sent jobs are done running. HOWEVER they will still
    /// be running. Be very careful to not see this as a "stop" button.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// let builder = ThreadPoolBuilder::with_max_threads()?;
    /// let mut pool = builder.build()?;
    ///
    /// pool.send_job(|| panic!("Test panic"));
    ///
    /// assert!(pool.wait_until_finished().is_err());
    /// assert!(pool.has_paniced());
    ///
    /// pool.reset_state();
    ///
    /// assert!(pool.wait_until_finished().is_ok());
    /// assert!(!pool.has_paniced());
    /// # Ok(())
    /// # }
    /// ```
    pub fn reset_state(&mut self) {
        let cvar = Arc::new(Condvar::new());
        let shared_state = Arc::new(SharedState::new());

        self.cvar = cvar;
        self.shared_state = shared_state;
    }

    /// This function will clone the threadpool and then reset its state. This
    /// makes it so you can have 2 different states operate on the same threads,
    /// effectively sharing the threads.
    ///
    /// Note however that there is no mechanism
    /// to give different instances equal CPU time, jobs are executed on a first
    /// come first server basis.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// let builder = ThreadPoolBuilder::with_max_threads()?;
    /// let pool = builder.build()?;
    ///
    /// let pool_clone = pool.clone_with_new_state();
    ///
    /// pool.send_job(|| panic!("Test panic"));
    ///
    /// assert!(pool.wait_until_finished().is_err());
    /// assert!(pool.has_paniced());
    ///
    /// assert!(pool_clone.wait_until_finished().is_ok());
    /// assert!(!pool_clone.has_paniced());
    /// # Ok(())
    /// # }
    /// ```
    pub fn clone_with_new_state(&self) -> Self {
        let mut new_pool = self.clone();
        new_pool.reset_state();
        new_pool
    }

    /// Returns the amount of jobs currently being ran by this instance of the
    /// thread pool. If muliple different instances of this threadpool (see [`clone_with_new_state`])
    /// this number might be lower than the max amount of threads, even if there
    /// are still jobs queued
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    /// use std::{
    ///     num::NonZeroUsize,
    ///     sync::{Arc, Barrier},
    /// };
    /// let threads = 16;
    /// let tasks = threads * 10;
    ///
    /// let num = NonZeroUsize::try_from(threads)?;
    /// let pool = ThreadPoolBuilder::with_thread_amount(num).build()?;
    ///
    /// let b0 = Arc::new(Barrier::new(threads + 1));
    /// let b1 = Arc::new(Barrier::new(threads + 1));
    ///
    /// for i in 0..tasks {
    ///     let b0_copy = b0.clone();
    ///     let b1_copy = b1.clone();
    ///
    ///     pool.send_job(move || {
    ///         if i < threads {
    ///             b0_copy.wait();
    ///             b1_copy.wait();
    ///         }
    ///     });
    /// }
    ///
    /// b0.wait();
    /// assert_eq!(pool.jobs_running(), threads);
    /// # b1.wait();
    /// # Ok(())
    /// # }
    /// ```
    pub fn jobs_running(&self) -> usize {
        self.shared_state.jobs_running.load(Ordering::Acquire)
    }

    /// Returns the amount of jobs currently queued by this threadpool instance.
    /// There might be more jobs queued that we don't know about if there are other
    /// instances of this threadpool (see [`clone_with_new_state`]).
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    /// use std::{
    ///     num::NonZeroUsize,
    ///     sync::{Arc, Barrier},
    /// };
    /// let threads = 16;
    /// let tasks = 100;
    ///
    /// let num = NonZeroUsize::try_from(threads)?;
    /// let pool = ThreadPoolBuilder::with_thread_amount(num).build()?;
    ///
    /// let b0 = Arc::new(Barrier::new(threads + 1));
    /// let b1 = Arc::new(Barrier::new(threads + 1));
    ///
    /// for i in 0..tasks {
    ///     let b0_copy = b0.clone();
    ///     let b1_copy = b1.clone();
    ///
    ///     pool.send_job(move || {
    ///         if i < threads {
    ///             b0_copy.wait();
    ///             b1_copy.wait();
    ///         }
    ///     });
    /// }
    ///
    /// b0.wait();
    /// assert_eq!(pool.jobs_queued(), tasks - threads);
    /// # b1.wait();
    /// # Ok(())
    /// # }
    /// ```
    pub fn jobs_queued(&self) -> usize {
        self.shared_state.jobs_queued.load(Ordering::Acquire)
    }

    /// Returns the amount of jobs that were sent by this instance of the threadpool
    /// and that paniced.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// let pool = ThreadPoolBuilder::with_max_threads()?.build()?;
    ///
    /// for i in 0..10 {
    ///     pool.send_job(|| panic!("Test panic"));
    /// }
    ///
    /// pool.wait_until_finished_unchecked();
    ///
    /// assert_eq!(pool.jobs_paniced(), 10);
    /// # Ok(())
    /// # }
    /// ```
    pub fn jobs_paniced(&self) -> usize {
        self.shared_state.jobs_paniced.load(Ordering::Acquire)
    }

    /// Returns whether a thread has had any jobs panic at all
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    ///
    /// let pool = ThreadPoolBuilder::with_max_threads()?.build()?;
    ///
    /// pool.send_job(|| panic!("Test panic"));
    ///
    /// pool.wait_until_finished_unchecked();
    ///
    /// assert!(pool.has_paniced());
    /// # Ok(())
    /// # }
    /// ```
    pub fn has_paniced(&self) -> bool {
        self.shared_state.has_paniced.load(Ordering::Acquire)
    }

    /// Returns whether a threadpool instance has no jobs running and no jobs queued,
    /// in other words if it's finished.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    /// use std::{
    ///     num::NonZeroUsize,
    ///     sync::{Arc, Barrier},
    /// };
    /// let pool = ThreadPoolBuilder::with_max_threads()?.build()?;
    ///
    /// let b = Arc::new(Barrier::new(2));
    ///
    /// assert!(pool.is_finished());
    ///
    /// let b_clone = b.clone();
    /// pool.send_job(move || { b_clone.wait(); });
    ///
    /// assert!(!pool.is_finished());
    /// # b.wait();
    /// # Ok(())
    /// # }
    /// ```
    pub fn is_finished(&self) -> bool {
        *self
            .shared_state
            .is_finished
            .lock()
            .expect("Shared state should never panic")
    }

    /// This function returns the amount of threads used to create the threadpool
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use easy_threadpool::ThreadPoolBuilder;
    /// use std::num::NonZeroUsize;
    ///
    /// let threads = 10;
    ///
    /// let num = NonZeroUsize::try_from(threads)?;
    /// let pool = ThreadPoolBuilder::with_thread_amount(num).build()?;
    ///
    /// assert_eq!(pool.threads().get(), threads);
    /// # Ok(())
    /// # }
    /// ```
    pub const fn threads(&self) -> NonZeroUsize {
        self.thread_amount
    }
}

/// A ThreadPoolbuilder is a builder to easily create a thread pool
pub struct ThreadPoolBuilder {
    thread_amount: NonZeroUsize,
    // thread_name: Option<String>,
}

impl Default for ThreadPoolBuilder {
    fn default() -> Self {
        Self {
            thread_amount: NonZeroUsize::try_from(1).unwrap(),
        }
    }
}

impl ThreadPoolBuilder {
    /// Initialize the amount of threads the builder will build to `thread_amount`
    pub fn with_thread_amount(thread_amount: NonZeroUsize) -> ThreadPoolBuilder {
        ThreadPoolBuilder { thread_amount }
    }

    /// Initialize the amount of threads the builder will build to `thread_amount`.
    ///
    /// # Errors
    ///
    /// If `thread_amount` cannot be converted to a [`std::num::NonZeroUsize`] (aka it is 0).
    pub fn with_thread_amount_usize(
        thread_amount: usize,
    ) -> Result<ThreadPoolBuilder, TryFromIntError> {
        let thread_amount = NonZeroUsize::try_from(thread_amount)?;
        Ok(Self::with_thread_amount(thread_amount))
    }

    /// Initialize the amount of threads the builder will build to the available parallelism
    /// as provided by [`std::thread::available_parallelism`]
    ///
    /// # Errors
    ///
    /// Taken from the available_parallelism() documentation:
    /// This function will, but is not limited to, return errors in the following
    /// cases:
    ///
    /// * If the amount of parallelism is not known for the target platform.
    /// * If the program lacks permission to query the amount of parallelism made
    ///   available to it.
    ///
    pub fn with_max_threads() -> io::Result<ThreadPoolBuilder> {
        let max_threads = available_parallelism()?;
        Ok(ThreadPoolBuilder {
            thread_amount: max_threads,
        })
    }

    // pub fn with_thread_name(thread_name: String) -> ThreadPoolBuilder {
    //     ThreadPoolBuilder {
    //         thread_name: Some(thread_name),
    //         ..Default::default()
    //     }
    // }

    /// Set the thead amount in the builder
    pub fn set_thread_amount(mut self, thread_amount: NonZeroUsize) -> ThreadPoolBuilder {
        self.thread_amount = thread_amount;
        self
    }

    /// Set the thead amount in the builder from usize
    ///
    /// # Errors
    ///
    /// If `thread_amount` cannot be turned into NonZeroUsize (aka it is 0)
    pub fn set_thread_amount_usize(
        self,
        thread_amount: usize,
    ) -> Result<ThreadPoolBuilder, TryFromIntError> {
        let thread_amount = NonZeroUsize::try_from(thread_amount)?;
        Ok(self.set_thread_amount(thread_amount))
    }

    /// set the amount of threads the builder will build to the available parallelism
    /// as provided by [`std::thread::available_parallelism`]
    ///
    /// # Errors
    ///
    /// Taken from the available_parallelism() documentation:
    /// This function will, but is not limited to, return errors in the following
    /// cases:
    ///
    /// * If the amount of parallelism is not known for the target platform.
    /// * If the program lacks permission to query the amount of parallelism made
    ///   available to it.
    ///
    pub fn set_max_threads(mut self) -> io::Result<ThreadPoolBuilder> {
        let max_threads = available_parallelism()?;
        self.thread_amount = max_threads;
        Ok(self)
    }

    // pub fn set_thread_name(mut self, thread_name: String) -> ThreadPoolBuilder {
    //     self.thread_name = Some(thread_name);
    //     self
    // }

    /// Build the builder into a threadpool, taking all the initialized values
    /// from the builder and using defaults for those not initialized.
    ///
    /// # Errors
    ///
    /// Taken from [`std::thread::Builder::spawn`]:
    ///
    /// Unlike the [`spawn`](https://doc.rust-lang.org/stable/std/thread/fn.spawn.html) free function, this method yields an
    /// [`io::Result`] to capture any failure to create the thread at
    /// the OS level.
    pub fn build(self) -> io::Result<ThreadPool> {
        ThreadPool::new(self)
    }
}

#[cfg(test)]
mod test {
    use core::panic;
    use std::{
        num::NonZeroUsize,
        sync::{mpsc::channel, Arc, Barrier},
        thread::sleep,
        time::Duration,
    };

    use crate::ThreadPoolBuilder;

    #[test]
    // Test multiple panics on a single thread, this ensures that a thread can
    // handle panics
    fn deal_with_panics() {
        fn panic_fn() {
            panic!("Test panic");
        }

        let thread_num: NonZeroUsize = 1.try_into().unwrap();
        let builder = ThreadPoolBuilder::with_thread_amount(thread_num);

        let pool = builder.build().unwrap();

        for _ in 0..10 {
            pool.send_job(panic_fn);
        }

        assert!(
            pool.wait_until_finished().is_err(),
            "Pool didn't detect panic in wait_until_finished"
        );

        assert!(
            pool.has_paniced(),
            "Pool didn't detect panic in has_paniced"
        );
        pool.wait_until_finished_unchecked();

        assert!(
            pool.jobs_queued() == 0,
            "Incorrect amount of jobs queued after wait"
        );
        assert!(
            pool.jobs_running() == 0,
            "Incorrect amount of jobs running after wait"
        );
        assert!(
            pool.jobs_paniced() == 10,
            "Incorrect amount of jobs paniced after wait"
        );
    }

    #[test]
    fn receive_value() {
        let (tx, rx) = channel::<u32>();

        let func = move || {
            tx.send(69).unwrap();
        };

        let pool = ThreadPoolBuilder::default().build().unwrap();

        pool.send_job(func);

        assert_eq!(rx.recv(), Ok(69), "Incorrect value received");
    }

    #[test]
    fn test_wait() {
        const TASKS: usize = 1000;
        const THREADS: usize = 16;

        let b0 = Arc::new(Barrier::new(THREADS + 1));
        let b1 = Arc::new(Barrier::new(THREADS + 1));

        let pool = ThreadPoolBuilder::with_thread_amount_usize(THREADS)
            .unwrap()
            .build()
            .unwrap();

        for i in 0..TASKS {
            let b0 = b0.clone();
            let b1 = b1.clone();

            pool.send_job(move || {
                if i < THREADS {
                    b0.wait();
                    b1.wait();
                }
            });
        }

        b0.wait();

        assert_eq!(
            pool.jobs_running(),
            THREADS,
            "Incorrect amount of jobs running"
        );
        assert_eq!(
            pool.jobs_paniced(),
            0,
            "Incorrect amount of threads paniced"
        );

        b1.wait();

        assert!(
            pool.wait_until_finished().is_ok(),
            "wait_until_finished incorrectly detected a panic"
        );

        assert_eq!(
            pool.jobs_queued(),
            0,
            "Incorrect amount of jobs queued after wait"
        );
        assert_eq!(
            pool.jobs_running(),
            0,
            "Incorrect amount of jobs running after wait"
        );
        assert_eq!(
            pool.jobs_paniced(),
            0,
            "Incorrect amount of threads paniced after wait"
        );
    }

    #[test]
    fn test_wait_unchecked() {
        const TASKS: usize = 1000;
        const THREADS: usize = 16;

        let b0 = Arc::new(Barrier::new(THREADS + 1));
        let b1 = Arc::new(Barrier::new(THREADS + 1));

        let builder = ThreadPoolBuilder::with_thread_amount_usize(THREADS).unwrap();
        let pool = builder.build().unwrap();

        for i in 0..TASKS {
            let b0 = b0.clone();
            let b1 = b1.clone();

            pool.send_job(move || {
                if i < THREADS {
                    b0.wait();
                    b1.wait();
                }
                panic!("Test panic");
            });
        }

        b0.wait();

        assert_eq!(
            pool.jobs_running(),
            THREADS,
            "Incorrect amount of jobs running"
        );
        assert_eq!(pool.jobs_paniced(), 0);

        b1.wait();

        pool.wait_until_finished_unchecked();

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), TASKS);
    }

    #[test]
    fn test_clones() {
        const TASKS: usize = 1000;
        const THREADS: usize = 16;

        let pool = ThreadPoolBuilder::with_thread_amount_usize(THREADS)
            .unwrap()
            .build()
            .unwrap();
        let clone = pool.clone();
        let clone_with_new_state = pool.clone_with_new_state();

        let b0 = Arc::new(Barrier::new(THREADS + 1));
        let b1 = Arc::new(Barrier::new(THREADS + 1));

        for i in 0..TASKS {
            let b0_copy = b0.clone();
            let b1_copy = b1.clone();

            pool.send_job(move || {
                if i < THREADS / 2 {
                    b0_copy.wait();
                    b1_copy.wait();
                }
            });

            let b0_copy = b0.clone();
            let b1_copy = b1.clone();

            clone_with_new_state.send_job(move || {
                if i < THREADS / 2 {
                    b0_copy.wait();
                    b1_copy.wait();
                }
                panic!("Test panic")
            });
        }

        b0.wait();

        // The /2 is guaranteed because jobs are received in order
        assert_eq!(
            pool.jobs_running(),
            THREADS / 2,
            "Incorrect amount of jobs running in pool"
        );
        assert_eq!(
            pool.jobs_paniced(),
            0,
            "Incorrect amount of jobs paniced in pool"
        );

        // The /2 is guaranteed because jobs are received in order
        assert_eq!(
            clone_with_new_state.jobs_running(),
            THREADS / 2,
            "Incorrect amount of jobs running in clone_with_new_state"
        );
        assert_eq!(
            clone_with_new_state.jobs_paniced(),
            0,
            "Incorrect amount of jobs paniced in clone_with_new_state"
        );

        b1.wait();
        assert!(
            clone_with_new_state.wait_until_finished().is_err(),
            "Clone with new state didn't detect panic"
        );

        assert!(
            clone.wait_until_finished().is_ok(),
            "Pool incorrectly detected panic"
        );

        assert_eq!(
            pool.jobs_queued(),
            0,
            "Incorrect amount of jobs queued in pool after wait"
        );
        assert_eq!(
            pool.jobs_running(),
            0,
            "Incorrect amount of jobs running in pool after wait"
        );
        assert_eq!(
            pool.jobs_paniced(),
            0,
            "Incorrect amount of jobs paniced in pool after wait"
        );

        clone_with_new_state.wait_until_finished_unchecked();
        assert!(
            clone_with_new_state.wait_until_finished().is_err(),
            "clone_with_new_state didn't detect panics after wait"
        );

        assert_eq!(
            clone_with_new_state.jobs_queued(),
            0,
            "Incorrect amount of jobs queued in clone_with_new_state after wait"
        );
        assert_eq!(
            clone_with_new_state.jobs_running(),
            0,
            "Incorrect amount of jobs running in clone_with_new_state after wait"
        );
        assert_eq!(
            clone_with_new_state.jobs_paniced(),
            TASKS,
            "Incorrect panics in clone"
        );

        assert_eq!(
            pool.jobs_queued(),
            0,
            "Incorrect amount of jobs queued in pool after everything"
        );
        assert_eq!(
            pool.jobs_running(),
            0,
            "Incorrect amount of jobs running in pool after everything"
        );
        assert_eq!(
            pool.jobs_paniced(),
            0,
            "Incorrect amount of jobs paniced in pool after everything"
        );
    }

    #[test]
    fn reset_state_while_running() {
        const TASKS: usize = 32;
        const THREADS: usize = 16;

        let mut pool = ThreadPoolBuilder::with_thread_amount_usize(THREADS)
            .unwrap()
            .build()
            .unwrap();

        let b0 = Arc::new(Barrier::new(THREADS + 1));
        let b1 = Arc::new(Barrier::new(THREADS + 1));

        for i in 0..TASKS {
            let b0_copy = b0.clone();
            let b1_copy = b1.clone();

            pool.send_job(move || {
                if i < THREADS {
                    b0_copy.wait();
                    b1_copy.wait();
                }
            });
        }

        b0.wait();

        assert_ne!(pool.jobs_queued(), 0);
        assert_ne!(pool.jobs_running(), 0);

        pool.reset_state();

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 0);

        b1.wait();
        pool.wait_until_finished().expect("Nothing should panic");

        // Give time for the jobs to execute
        sleep(Duration::from_secs(1));

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 0);
    }

    #[test]
    fn reset_panic_test() {
        const TASKS: usize = 32;
        const THREADS: usize = 16;

        let num = NonZeroUsize::try_from(THREADS).unwrap();
        let mut pool = ThreadPoolBuilder::with_thread_amount(num).build().unwrap();

        let b0 = Arc::new(Barrier::new(THREADS + 1));
        let b1 = Arc::new(Barrier::new(THREADS + 1));

        for i in 0..TASKS {
            let b0_copy = b0.clone();
            let b1_copy = b1.clone();

            pool.send_job(move || {
                if i < THREADS {
                    b0_copy.wait();
                    b1_copy.wait();
                }
                panic!("Test panic");
            });
        }

        b0.wait();

        assert_ne!(pool.jobs_queued(), 0);
        assert_ne!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 0);

        pool.reset_state();

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 0);

        b1.wait();
        pool.wait_until_finished().expect("Nothing should panic");

        // Give time for the jobs to execute
        sleep(Duration::from_secs(1));

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 0);
    }

    #[test]
    fn test_wait_until_job_done() {
        const THREADS: usize = 1;

        let builder = ThreadPoolBuilder::with_thread_amount_usize(THREADS).unwrap();
        let pool = builder.build().unwrap();

        assert!(pool.wait_until_job_done().is_ok());

        pool.send_job(|| {});

        assert!(pool.wait_until_job_done().is_ok());

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 0);

        pool.send_job(|| panic!("Test panic"));

        assert!(pool.wait_until_job_done().is_err());

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 1);
    }

    #[test]
    fn test_wait_until_job_done_unchecked() {
        const THREADS: usize = 1;

        let builder = ThreadPoolBuilder::with_thread_amount_usize(THREADS).unwrap();
        let pool = builder.build().unwrap();

        // This doesn't block forever
        pool.wait_until_job_done_unchecked();

        pool.send_job(|| {});

        pool.wait_until_job_done_unchecked();

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 0);

        pool.send_job(|| panic!("Test panic"));

        pool.wait_until_job_done_unchecked();

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.jobs_paniced(), 1);
    }

    #[test]
    #[allow(dead_code)]
    fn test_flakiness() {
        for _ in 0..10 {
            test_wait();
            test_wait_unchecked();
            deal_with_panics();
            receive_value();
            test_clones();
            reset_state_while_running();
            test_wait_until_job_done_unchecked();
            test_wait_until_job_done();
            reset_panic_test();
        }
    }
}
