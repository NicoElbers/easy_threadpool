use std::{
    fmt::Display,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};

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
