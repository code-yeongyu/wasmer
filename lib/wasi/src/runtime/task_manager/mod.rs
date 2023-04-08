// TODO: should be behind a different , tokio specific feature flag.
#[cfg(feature = "sys-thread")]
pub mod tokio;

use std::cell::RefCell;
use std::task::{Context, Poll};
use std::{pin::Pin, time::Duration};

use ::tokio::runtime::Handle;
use futures::Future;
use wasmer::{Memory, MemoryType, Module, Store, StoreMut};
use wasmer_wasix_types::wasi::{Errno, ExitCode};

use crate::os::task::thread::WasiThreadError;
use crate::syscalls::AsyncifyFuture;
use crate::WasiFunctionEnv;

#[derive(Debug)]
pub struct SpawnedMemory {
    pub ty: MemoryType,
}

#[derive(Debug)]
pub enum SpawnType {
    Create,
    CreateWithType(SpawnedMemory),
    NewThread(Memory),
}

/// Indicates if the task should run with the supplied store
/// or if it should abort and exit the thread
pub enum TaskResumeAction {
    // The task will run with the following store
    Run(Store, Result<(), Errno>),
    /// The task has been aborted
    Abort,
}

pub type WasmResumeTask = dyn FnOnce(Store, Result<(), Errno>) + Send + 'static;

pub type WasmResumeTrigger = dyn FnOnce(Store) -> Pin<Box<dyn Future<Output = TaskResumeAction> + Send + 'static>>
    + Send
    + Sync;

/// An implementation of task management
#[allow(unused_variables)]
pub trait VirtualTaskManager: std::fmt::Debug + Send + Sync + 'static {
    /// Build a new Webassembly memory.
    ///
    /// May return `None` if the memory can just be auto-constructed.
    fn build_memory(
        &self,
        store: &mut StoreMut,
        spawn_type: SpawnType,
    ) -> Result<Option<Memory>, WasiThreadError>;

    /// Invokes whenever a WASM thread goes idle. In some runtimes (like singlethreaded
    /// execution environments) they will need to do asynchronous work whenever the main
    /// thread goes idle and this is the place to hook for that.
    fn sleep_now(
        &self,
        time: Duration,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>;

    /// Starts an asynchronous task that will run on a shared worker pool
    /// This task must not block the execution or it could cause a deadlock
    fn task_shared(
        &self,
        task: Box<
            dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + 'static,
        >,
    ) -> Result<(), WasiThreadError>;

    /// Returns a runtime that can be used for asynchronous tasks
    fn runtime(&self) -> &Handle;

    /// Enters a runtime context
    #[allow(dyn_drop)]
    fn runtime_enter<'g>(&'g self) -> Box<dyn std::ops::Drop + 'g>;

    /// Starts an WebAssembly task will will run on a dedicated thread
    /// pulled from the worker pool that has a stateful thread local variable
    fn task_wasm(
        &self,
        task: Box<dyn FnOnce(Store, Module, Option<Memory>) + Send + 'static>,
        store: Store,
        module: Module,
        spawn_type: SpawnType,
    ) -> Result<(), WasiThreadError>;

    /// Starts an WebAssembly task will will run on a dedicated thread
    /// pulled from the worker pool that has a stateful thread local variable
    /// After the trigger has successfully completed
    fn resume_wasm_after_trigger(
        &self,
        task: Box<WasmResumeTask>,
        store: Store,
        module: Module,
        memory: Memory,
        trigger: Box<WasmResumeTrigger>,
    ) -> Result<(), WasiThreadError>;

    /// Starts an asynchronous task will will run on a dedicated thread
    /// pulled from the worker pool. It is ok for this task to block execution
    /// and any async futures within its scope
    fn task_dedicated(
        &self,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<(), WasiThreadError>;

    /// Returns the amount of parallelism that is possible on this platform
    fn thread_parallelism(&self) -> Result<usize, WasiThreadError>;
}

impl dyn VirtualTaskManager {
    /// Execute a future and return the output.
    /// This method blocks until the future is complete.
    // This needs to be a generic impl on `dyn T` because it is generic, and hence not object-safe.
    pub fn block_on<'a, A>(&self, task: impl Future<Output = A> + 'a) -> A {
        self.runtime().block_on(task)
    }

    /// Starts an WebAssembly task will will run on a dedicated thread
    /// pulled from the worker pool that has a stateful thread local variable
    /// After the poller has successed
    pub fn resume_wasm_after_poller(
        &self,
        task: Box<WasmResumeTask>,
        store: Store,
        env: WasiFunctionEnv,
        work: Box<dyn AsyncifyFuture + Send + Sync + 'static>,
    ) -> Result<(), WasiThreadError> {
        // Extract the module and memory
        let module = env.data(&store).inner().instance.module().clone();
        let memory = env.data(&store).memory_clone();

        // This poller will process any signals when the main working function is idle
        struct AsyncifyPollerOwned {
            env: WasiFunctionEnv,
            store: Store,
            work: RefCell<Box<dyn AsyncifyFuture + Send + Sync + 'static>>,
        }
        impl Future for AsyncifyPollerOwned {
            type Output = Result<Result<(), Errno>, ExitCode>;
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut work = self.work.borrow_mut();
                let store = &self.store;
                let env = self.env.data(store);

                Poll::Ready(if let Poll::Ready(res) = work.poll(env, &self.store, cx) {
                    Ok(res)
                } else if let Some(exit_code) = env.should_exit() {
                    Err(exit_code)
                } else if env.thread.has_signals_or_subscribe(cx.waker()) {
                    Ok(Err(Errno::Intr))
                } else {
                    return Poll::Pending;
                })
            }
        }

        self.resume_wasm_after_trigger(
            task,
            store,
            module,
            memory,
            Box::new(move |store| {
                Box::pin(async move {
                    let mut poller = AsyncifyPollerOwned {
                        env,
                        store,
                        work: RefCell::new(work),
                    };
                    let res = Pin::new(&mut poller).await;
                    let res = match res {
                        Ok(res) => res,
                        Err(exit_code) => {
                            let env = poller.env.data(&poller.store);
                            env.thread.set_status_finished(Ok(exit_code));
                            return TaskResumeAction::Abort;
                        }
                    };
                    tracing::trace!("deep sleep woken - {:?}", res);
                    TaskResumeAction::Run(poller.store, res)
                })
            }),
        )
    }
}

/// Generic utility methods for VirtualTaskManager
pub trait VirtualTaskManagerExt {
    fn block_on<'a, A>(&self, task: impl Future<Output = A> + 'a) -> A;
}

impl<'a, T: VirtualTaskManager> VirtualTaskManagerExt for &'a T {
    fn block_on<'x, A>(&self, task: impl Future<Output = A> + 'x) -> A {
        self.runtime().block_on(task)
    }
}

impl<T: VirtualTaskManager + ?Sized> VirtualTaskManagerExt for std::sync::Arc<T> {
    fn block_on<'x, A>(&self, task: impl Future<Output = A> + 'x) -> A {
        self.runtime().block_on(task)
    }
}
