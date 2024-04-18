use std::{
    error::Error,
    fmt::Display,
    io,
    num::NonZeroUsize,
    panic::UnwindSafe,
    sync::{
        mpsc::{self, channel, Sender},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread::{self, available_parallelism, current},
};

type ThreadPoolFunctionBoxed = Box<dyn FnOnce() + Send + UnwindSafe>;

#[derive(Debug)]
pub struct JobHasPanicedError {}

impl Display for JobHasPanicedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "At least one job in the threadpool has caused a panic")
    }
}

impl Error for JobHasPanicedError {}

struct SharedState {
    jobs_queued: usize,
    jobs_running: usize,
    jobs_paniced: usize,
}

impl Display for SharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SharedState<jobs_queued: {}, jobs_running: {}, jobs_paniced: {}>",
            self.jobs_queued, self.jobs_running, self.jobs_paniced
        )
    }
}

impl SharedState {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            jobs_running: 0,
            jobs_queued: 0,
            jobs_paniced: 0,
        }))
    }

    fn job_starting(&mut self) {
        debug_assert!(self.jobs_queued > 0, "Negative jobs queued");

        self.jobs_queued -= 1;
        self.jobs_running += 1;
    }

    fn job_finished(&mut self) {
        debug_assert!(self.jobs_running > 0, "Negative jobs running");

        self.jobs_running -= 1;
    }

    fn job_queued(&mut self) {
        self.jobs_queued += 1;
    }

    fn job_paniced(&mut self) {
        self.jobs_paniced += 1;
    }
}

pub struct ThreadPool {
    thread_amount: NonZeroUsize,
    job_sender: Arc<Sender<ThreadPoolFunctionBoxed>>,
    shared_state: Arc<Mutex<SharedState>>,
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
        let state = self
            .shared_state
            .lock()
            .expect("Threadpool shared state has paniced");

        write!(
            f,
            "Threadpool< thread_amount: {}, shared_state: {}>",
            self.thread_amount, state
        )
    }
}

impl ThreadPool {
    fn new(builder: ThreadPoolBuilder) -> io::Result<Self> {
        let thread_amount = match builder.thread_amount {
            Some(amount) => amount,
            None => available_parallelism()?,
        };

        let (job_sender, job_receiver) = channel::<ThreadPoolFunctionBoxed>();
        let job_sender = Arc::new(job_sender);
        let shareable_job_reciever = Arc::new(Mutex::new(job_receiver));

        let shared_state = SharedState::new();
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

    pub fn send_job(
        &mut self,
        job: impl FnOnce() + Send + Sync + UnwindSafe + 'static,
    ) -> Result<(), mpsc::SendError<ThreadPoolFunctionBoxed>> {
        // NOTE: It is essential that the shared state is updated FIRST otherwise
        // we have a race condidition that the job is transmitted and read before
        // the shared state is updated, leading to a negative amount of jobs queued
        self.shared_state
            .lock()
            .expect("Threadpool shared state has paniced")
            .job_queued();

        // Pass our own state to the job. This makes it so that multiple threadpools
        // with different states can send jobs to the same threads without getting
        // eachothers panics for example
        let state = self.shared_state.clone();
        let cvar = self.cvar.clone();
        let job_with_state = Self::job_function(Box::new(job), state, cvar);

        self.job_sender.send(Box::new(job_with_state))
    }

    pub fn wait_until_finished(&self) -> Result<(), JobHasPanicedError> {
        fn finished(guard: &MutexGuard<SharedState>) -> bool {
            guard.jobs_running == 0 && guard.jobs_queued == 0
        }
        fn paniced(guard: &MutexGuard<SharedState>) -> bool {
            guard.jobs_paniced != 0
        }

        let mut guard = self
            .shared_state
            .lock()
            .expect("Threadpool shared state has paniced");

        while !finished(&guard) && !paniced(&guard) {
            guard = self
                .cvar
                .wait(guard)
                .expect("Threadpool shared state has paniced");
        }

        // Keep the guard to not have to relock later
        if paniced(&guard) {
            Err(JobHasPanicedError {})
        } else {
            Ok(())
        }
    }

    pub fn wait_until_finished_unchecked(&self) {
        fn finished(guard: &MutexGuard<SharedState>) -> bool {
            guard.jobs_running == 0 && guard.jobs_queued == 0
        }

        let mut guard = self
            .shared_state
            .lock()
            .expect("Threadpool shared state has paniced");

        while !finished(&guard) {
            guard = self
                .cvar
                .wait(guard)
                .expect("Threadpool shared state has paniced");
        }
    }

    pub fn reset_state(&mut self) {
        let cvar = Arc::new(Condvar::new());
        let shared_state = SharedState::new();

        self.cvar = cvar;
        self.shared_state = shared_state;
    }

    pub fn clone_with_new_state(&self) -> Self {
        let mut new_pool = self.clone();
        new_pool.reset_state();
        new_pool
    }

    pub fn jobs_running(&self) -> usize {
        self.shared_state
            .lock()
            .expect("Threadpool shared state has paniced")
            .jobs_running
    }

    pub fn jobs_queued(&self) -> usize {
        self.shared_state
            .lock()
            .expect("Threadpool shared state has paniced")
            .jobs_queued
    }

    pub fn threads_paniced(&self) -> usize {
        self.shared_state
            .lock()
            .expect("Threadpool shared state has paniced")
            .jobs_paniced
    }

    pub fn has_paniced(&self) -> bool {
        self.threads_paniced() != 0
    }

    pub fn is_finished(&self) -> bool {
        self.jobs_running() == 0 && self.jobs_queued() == 0
    }

    pub const fn threads(&self) -> NonZeroUsize {
        self.thread_amount
    }

    fn job_function(
        job: ThreadPoolFunctionBoxed,
        state: Arc<Mutex<SharedState>>,
        cvar: Arc<Condvar>,
    ) -> impl FnOnce() + Send + 'static {
        move || {
            state
                .lock()
                .expect("Threadpool shared state has paniced")
                .job_starting();

            // NOTE: The use of catch_unwind means that the thread will not
            // panic from any of the jobs it was sent. This is useful because
            // we won't ever have to restart a thread.
            let result = std::panic::catch_unwind(job);

            // NOTE: Do the panic check first otherwise we have a race condition
            // where the final job panics and the wait_until_finished function
            // doesn't detect it
            if result.is_err() {
                state
                    .lock()
                    .expect("Threadpool shared state has paniced")
                    .job_paniced();

                eprintln!(
                    "Job paniced: Thread \"{}\" is panicing",
                    current().name().unwrap_or("Unnamed worker")
                );
            }

            state
                .lock()
                .expect("Threadpool shared state has paniced")
                .job_finished();

            cvar.notify_all();
        }
    }
}

#[derive(Default)]
pub struct ThreadPoolBuilder {
    thread_amount: Option<NonZeroUsize>,
    thread_name: Option<String>,
}

impl ThreadPoolBuilder {
    pub fn with_thread_amount(thread_amount: NonZeroUsize) -> ThreadPoolBuilder {
        ThreadPoolBuilder {
            thread_amount: Some(thread_amount),
            ..Default::default()
        }
    }

    pub fn with_max_threads() -> io::Result<ThreadPoolBuilder> {
        let max_threads = available_parallelism()?;
        Ok(ThreadPoolBuilder {
            thread_amount: Some(max_threads),
            ..Default::default()
        })
    }

    pub fn with_thread_name(thread_name: String) -> ThreadPoolBuilder {
        ThreadPoolBuilder {
            thread_name: Some(thread_name),
            ..Default::default()
        }
    }

    pub fn set_thread_amount(mut self, thread_amount: NonZeroUsize) -> ThreadPoolBuilder {
        self.thread_amount = Some(thread_amount);
        self
    }

    pub fn set_max_threads(mut self) -> io::Result<ThreadPoolBuilder> {
        let max_threads = available_parallelism()?;
        self.thread_amount = Some(max_threads);
        Ok(self)
    }

    pub fn set_thread_name(mut self, thread_name: String) -> ThreadPoolBuilder {
        self.thread_name = Some(thread_name);
        self
    }

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

        let mut pool = builder.build().unwrap();

        for _ in 0..10 {
            pool.send_job(panic_fn).unwrap();
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
            pool.threads_paniced() == 10,
            "Incorrect amount of jobs paniced after wait"
        );
    }

    #[test]
    fn receive_value() {
        let (tx, rx) = channel::<u32>();

        let func = move || {
            tx.send(69).unwrap();
        };

        let mut pool = ThreadPoolBuilder::default().build().unwrap();

        pool.send_job(func).unwrap();

        assert_eq!(rx.recv(), Ok(69), "Incorrect value received");
    }

    #[test]
    fn test_wait() {
        const TASKS: usize = 1000;
        const THREADS: usize = 16;

        let b0 = Arc::new(Barrier::new(THREADS + 1));
        let b1 = Arc::new(Barrier::new(THREADS + 1));

        let mut pool = ThreadPoolBuilder::default().build().unwrap();

        for i in 0..TASKS {
            let b0 = b0.clone();
            let b1 = b1.clone();

            pool.send_job(move || {
                if i < THREADS {
                    b0.wait();
                    b1.wait();
                }
            })
            .unwrap();
        }

        b0.wait();

        assert_eq!(
            pool.jobs_running(),
            THREADS,
            "Incorrect amount of jobs running"
        );
        assert_eq!(
            pool.threads_paniced(),
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
            pool.threads_paniced(),
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

        let mut pool = ThreadPoolBuilder::default().build().unwrap();

        for i in 0..TASKS {
            let b0 = b0.clone();
            let b1 = b1.clone();

            pool.send_job(move || {
                if i < THREADS {
                    b0.wait();
                    b1.wait();
                }
                panic!("Test panic");
            })
            .unwrap();
        }

        b0.wait();

        assert_eq!(
            pool.jobs_running(),
            THREADS,
            "Incorrect amount of jobs running"
        );
        assert_eq!(pool.threads_paniced(), 0);

        b1.wait();

        pool.wait_until_finished_unchecked();

        assert_eq!(pool.jobs_queued(), 0);
        assert_eq!(pool.jobs_running(), 0);
        assert_eq!(pool.threads_paniced(), TASKS);
    }

    #[test]
    fn test_clones() {
        const TASKS: usize = 1000;
        const THREADS: usize = 16;

        let mut pool = ThreadPoolBuilder::default().build().unwrap();
        let clone = pool.clone();
        let mut clone_with_new_state = pool.clone_with_new_state();

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
            })
            .unwrap();

            let b0_copy = b0.clone();
            let b1_copy = b1.clone();

            clone_with_new_state
                .send_job(move || {
                    if i < THREADS / 2 {
                        b0_copy.wait();
                        b1_copy.wait();
                    }
                    panic!("Test panic")
                })
                .unwrap();
        }

        b0.wait();

        // The /2 is guaranteed because jobs are received in order
        assert_eq!(
            pool.jobs_running(),
            THREADS / 2,
            "Incorrect amount of jobs running in pool"
        );
        assert_eq!(
            pool.threads_paniced(),
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
            clone_with_new_state.threads_paniced(),
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
            pool.threads_paniced(),
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
            clone_with_new_state.threads_paniced(),
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
            pool.threads_paniced(),
            0,
            "Incorrect amount of jobs paniced in pool after everything"
        );
    }

    #[test]
    fn test_flakiness() {
        for _ in 0..10000 {
            test_wait();
            test_wait_unchecked();
            deal_with_panics();
            receive_value();
            test_clones();
        }
    }
}
