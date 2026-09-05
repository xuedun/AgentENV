//! IoRingWorker is a wrapper that doing IO request, async and using io uring under the hood.
//!
//! Each worker will spawn a new thread and create a current_thread tokio runtime.
//! The request is handled using a mpsc channel.
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::any::Any;
use std::cell::OnceCell;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::{mpsc, oneshot};

use super::{
    AsyncIoRing, AsyncIoRingBuilder, AsyncIoRingSQE, IoUringSubmitter, IoUringSubmitterSendBox,
};

thread_local! {
    /// This is a thread-local uring that will be initialized for each [IoRingWorker].
    /// It can only be access in the thread of [IoRingWorker].
    ///
    /// This io uring has enabled sparse files, so you can call [register_fds][AsyncIoRing::register_fds]
    /// on it, if you want to.
    pub static URING: OnceCell<AsyncIoRing> = const {OnceCell::new()};
    pub static URING128: OnceCell<AsyncIoRing<io_uring::squeue::Entry128>> = const {OnceCell::new()};
}

#[async_trait(?Send)]
pub trait TlsRing: Sized {
    /// Return the thread-local uring that will only be initialized for AsyncIoRing
    /// provided by [IoRingWorker].
    fn tls_ring() -> &'static std::thread::LocalKey<OnceCell<Self>>;
    async fn unregister_fd(&self, idx: u32) -> Result<()>;
}

#[async_trait(?Send)]
impl TlsRing for AsyncIoRing<io_uring::squeue::Entry> {
    fn tls_ring() -> &'static std::thread::LocalKey<OnceCell<AsyncIoRing>> {
        &URING
    }

    async fn unregister_fd(&self, idx: u32) -> Result<()> {
        self.unregister_fd(idx).await
    }
}

#[async_trait(?Send)]
impl TlsRing for AsyncIoRing<io_uring::squeue::Entry128> {
    fn tls_ring(
    ) -> &'static std::thread::LocalKey<OnceCell<AsyncIoRing<io_uring::squeue::Entry128>>> {
        &URING128
    }

    async fn unregister_fd(&self, _idx: u32) -> Result<()> {
        bail!("unregister_fd is not implemented for AsyncIoRing<Entry128>");
    }
}

pub trait RegisterableBuf: Send + Sync + Any + AsMut<[u8]> {}
impl<T> RegisterableBuf for T where T: Any + AsMut<[u8]> + Send + Sync {}

enum IoRingRequest<S: AsyncIoRingSQE = io_uring::squeue::Entry128> {
    Io {
        entry: S,
        tx: oneshot::Sender<std::io::Result<i32>>,
    },
    OccupySparseBuffer {
        /// The index in the io uring's sparse buffer table
        tx: oneshot::Sender<Result<u32>>,
    },
    /// Release means give the index of previously `OccupySparseBuffer` index
    /// back the io uring.
    ReleaseSparseBuffer {
        /// The index in the io uring's sparse file table
        idx: u32,
        tx: oneshot::Sender<Result<()>>,
    },
    RegisterBuffer {
        /// An owned version of buffer
        buf: Box<dyn RegisterableBuf>,
        /// will send the register buffer back
        tx: oneshot::Sender<(Result<u32>, Box<dyn RegisterableBuf>)>,
    },
    /// Fire-and-forget variant of [`ReleaseSparseBuffer`] – safe to call from
    /// `Drop` because it never blocks or awaits.
    ReleaseSparseBufferFire { idx: u32 },
    /// Unregister means unregister the buffer of previously `RegisterBuffer`.
    /// The unregister_buffer is
    UnregisterBuffer { idx: u32 },
}

/// A wrapper over io uring worker, running on a single thread tokio runtime
pub struct IoRingWorker<S: AsyncIoRingSQE> {
    /// Worker ID
    pub worker_id: usize,
    /// Request receiver
    request_rx: mpsc::UnboundedReceiver<IoRingRequest<S>>,
}

/// A handle that used to communicate with IoRingWorker
#[derive(Clone, Debug)]
pub struct IoRingHandle<S: AsyncIoRingSQE = io_uring::squeue::Entry> {
    pub worker_id: usize,
    request_tx: mpsc::UnboundedSender<IoRingRequest<S>>,
}

/// Spawn a new thread, create an IoRingWorker and a current_thread tokio runtime.
pub fn spawn_io_ring_worker<S>(worker_id: usize) -> (IoRingHandle<S>, std::thread::JoinHandle<()>)
where
    S: AsyncIoRingSQE,
    AsyncIoRing<S>: TlsRing,
{
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let handle = std::thread::spawn(move || {
        unsafe {
            const PR_SET_IO_FLUSHER: i32 = 57; // include/uapi/linux/prctl.h

            // ignore the error
            let _res = libc::prctl(PR_SET_IO_FLUSHER, 0, 0, 0, 0);
        }
        // TODO: set thread affinity

        let worker = match IoRingWorker::new(worker_id, request_rx) {
            Ok(worker) => worker,
            Err(err) => {
                tracing::error!(?err, "create io ring worker failed");
                return;
            }
        };
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .on_thread_park(move || {
                let tls_ring = AsyncIoRing::<S>::tls_ring();
                tls_ring.with(|uring| {
                    // if submit via uring failed, we just panic for now, since there
                    // is no easy fallback, currently.
                    if let Some(ring) = uring.get() {
                        let _ = ring
                            .borrow()
                            .submit()
                            .with_context(|| format!("worker {worker_id} submit for io uring"))
                            .inspect_err(|err| {
                                tracing::error!(?err, "on_thread_park callback failed")
                            });
                    }
                })
            })
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                tracing::error!(?err, "build tokio runtime for io ring worker failed");
                return;
            }
        };
        let local_set = tokio::task::LocalSet::new();
        local_set.block_on(&rt, worker.run());
        tracing::warn!(worker_id, "io ring worker has exited");
    });
    (
        IoRingHandle {
            worker_id,
            request_tx,
        },
        handle,
    )
}

impl<S> IoRingWorker<S>
where
    S: AsyncIoRingSQE,
    AsyncIoRing<S>: TlsRing,
{
    /// Spawn
    fn new(
        worker_id: usize,
        request_rx: mpsc::UnboundedReceiver<IoRingRequest<S>>,
    ) -> Result<Self> {
        // FIXME: how to set the nr_sparse_file limit?
        // FIXME: different args for different S (128 cal use smaller value)
        //
        // #define IORING_MAX_FIXED_FILES	(1U << 20)
        // #define IORING_MAX_REG_BUFFERS	(1U << 14)
        let ring = AsyncIoRingBuilder::<S>::new()
            .nr_sparse_buffer(4096)
            .sqe_entries(2048)
            .cqe_entries(4096)
            .build()
            .with_context(|| format!("create io uring for worker {worker_id}"))?;
        // init the uring
        if AsyncIoRing::<S>::tls_ring()
            .with(|uring| uring.set(ring.clone()))
            .is_err()
        {
            bail!("uring for worker {worker_id} has already been initialized");
        }

        Ok(Self {
            worker_id,
            request_rx,
        })
    }

    /// Run the worker event loop
    pub async fn run(mut self) {
        tracing::info!(worker_id = self.worker_id, "IoRingWorker main loop started");
        let Some(uring) = AsyncIoRing::<S>::tls_ring().with(|uring| uring.get().cloned()) else {
            panic!(
                "IoRingWorker {} uring has not been initialized",
                self.worker_id
            );
        };
        let worker_id = self.worker_id;
        tokio::task::spawn_local({
            let uring = uring.clone();
            async move {
                loop {
                    if let Err(err) = uring.handle_completion().await {
                        tracing::error!(
                            worker_id,
                            ?err,
                            "handle_completion exited, restarting after 1s"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        });

        while let Some(request) = self.request_rx.recv().await {
            match request {
                IoRingRequest::Io { entry, tx } => {
                    let uring = uring.clone();
                    tokio::task::spawn_local(async move {
                        let res = match uring.send(entry) {
                            Ok(fut) => Ok(fut.await),
                            Err(err) => Err(err),
                        };
                        let _ = tx.send(res);
                    });
                }
                IoRingRequest::OccupySparseBuffer { tx } => {
                    let res = uring.occupy_sparse_buffer_index();
                    let _ = tx.send(res);
                }
                IoRingRequest::ReleaseSparseBuffer { idx, tx } => {
                    let res = uring.release_sparse_buffer_index(idx);
                    let _ = tx.send(res);
                }
                IoRingRequest::ReleaseSparseBufferFire { idx } => {
                    if let Err(err) = uring.release_sparse_buffer_index(idx) {
                        tracing::error!(?err, idx, "release_sparse_buffer_index failed");
                    }
                }
                IoRingRequest::RegisterBuffer { mut buf, tx } => {
                    let res = {
                        let buf = buf.as_mut().as_mut();
                        uring.register_buffer(buf)
                    };
                    let _ = tx.send((res, buf));
                }
                IoRingRequest::UnregisterBuffer { idx } => {
                    if let Err(err) = uring.unregister_buffer(idx) {
                        tracing::error!(?err, idx, "unregister_buffer failed");
                    }
                }
            }
        }
    }
}

impl<S> IoRingHandle<S>
where
    S: AsyncIoRingSQE,
    AsyncIoRing<S>: TlsRing,
{
    pub async fn occupy_sparse_buffer_index(&self) -> Result<u32> {
        let (tx, rx) = oneshot::channel();
        let req = IoRingRequest::OccupySparseBuffer { tx };
        self.request_tx
            .send(req)
            .context("send request over IoRingHandle channel")?;
        rx.await
            .context("wait for occupy_sparse_buffer_index response")?
    }

    pub async fn release_sparse_buffer_index(&self, idx: u32) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let req = IoRingRequest::ReleaseSparseBuffer { tx, idx };
        self.request_tx
            .send(req)
            .context("send request over IoRingHandle channel")?;
        rx.await
            .context("wait for release_sparse_buffer_index response")?
    }

    /// Tell the io_uring worker to release a sparse buffer index.
    /// Fire-and-forget: the release may not have completed when this
    /// returns.  Safe to call from `Drop` (no blocking, no async).
    pub fn fire_release_sparse_buffer_index(&self, idx: u32) -> Result<()> {
        let req = IoRingRequest::ReleaseSparseBufferFire { idx };
        self.request_tx
            .send(req)
            .context("send request over IoRingHandle channel")?;
        Ok(())
    }

    pub async fn register_buffer(
        &self,
        buf: Box<dyn RegisterableBuf>,
    ) -> Result<(u32, Box<dyn RegisterableBuf>)> {
        let (tx, rx) = oneshot::channel();
        let req = IoRingRequest::RegisterBuffer { tx, buf };
        self.request_tx
            .send(req)
            .context("send request over IoRingHandle channel")?;
        let (res, buf) = rx.await.context("wait for register_buffer response")?;
        res.map(|idx| (idx, buf))
    }

    /// Just tell the io uring worker to unregister the buffer. When this function returns, the
    /// buffer might not be unregistered (best effort). Typically, used in `Drop`.
    pub fn fire_unregister_buffer(&self, idx: u32) -> Result<()> {
        let req = IoRingRequest::UnregisterBuffer { idx };
        self.request_tx
            .send(req)
            .context("send request over IoRingHandle channel")?;
        Ok(())
    }

    pub async fn send_io(&self, entry: S) -> std::io::Result<i32> {
        let (tx, rx) = oneshot::channel();
        let req = IoRingRequest::Io { entry, tx };
        self.request_tx.send(req).map_err(std::io::Error::other)?;
        rx.await.map_err(std::io::Error::other)?
    }
}

impl IoUringSubmitter for IoRingHandle {
    fn submit<'a>(
        &'a self,
        entry: io_uring::squeue::Entry,
    ) -> impl Future<Output = std::io::Result<i32>> + 'a {
        self.send_io(entry)
    }
}

impl IoUringSubmitterSendBox for IoRingHandle {
    fn submit_box<'a>(
        &'a self,
        entry: io_uring::squeue::Entry,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<i32>> + Send + 'a>> {
        Box::pin(self.send_io(entry))
    }
}
