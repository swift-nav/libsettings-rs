use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

pub struct Context {
    pub(crate) cancel_rx: Receiver<()>,
    pub(crate) timeout_rx: Receiver<Instant>,
    timeout_at: Instant,
    shared: Arc<Shared>,
}

impl Context {
    pub fn new() -> (Context, CtxHandle) {
        let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded();
        let timeout_rx = crossbeam_channel::never();
        let timeout_at = Instant::now() + Duration::from_secs(u64::MAX);
        let shared = Arc::new(Shared {
            cancel_tx,
            num_chans: AtomicUsize::new(1),
        });
        (
            Context {
                cancel_rx,
                timeout_rx,
                timeout_at,
                shared: Arc::clone(&shared),
            },
            CtxHandle { shared },
        )
    }

    pub fn with_timeout(timeout: Duration) -> (Context, CtxHandle) {
        let (mut ctx, h) = Self::new();
        ctx.set_timeout(timeout);
        (ctx, h)
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout_at = Instant::now() + timeout;
        self.timeout_rx = crossbeam_channel::at(self.timeout_at);
    }

    pub fn cancel(&self) {
        self.shared.cancel();
    }
}

impl Clone for Context {
    fn clone(&self) -> Self {
        self.shared.num_chans.fetch_add(1, Ordering::SeqCst);
        Self {
            cancel_rx: self.cancel_rx.clone(),
            timeout_rx: crossbeam_channel::at(self.timeout_at),
            timeout_at: self.timeout_at,
            shared: Arc::clone(&self.shared),
        }
    }
}

pub struct CtxHandle {
    shared: Arc<Shared>,
}

impl CtxHandle {
    pub fn cancel(&self) {
        self.shared.cancel();
    }
}

struct Shared {
    cancel_tx: Sender<()>,
    num_chans: AtomicUsize,
}

impl Shared {
    fn cancel(&self) {
        for _ in 0..self.num_chans.load(Ordering::SeqCst) {
            let _ = self.cancel_tx.try_send(());
        }
    }
}
