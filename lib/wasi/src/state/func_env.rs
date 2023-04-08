use tracing::trace;
use wasmer::{
    AsStoreMut, AsStoreRef, ExportError, FunctionEnv, Imports, Instance, InstantiationError,
    Memory, Module, Store,
};
use wasmer_wasix_types::wasi::ExitCode;

use crate::{
    os::task::thread::DEFAULT_STACK_SIZE,
    state::WasiInstanceHandles,
    utils::{get_wasi_version, get_wasi_versions},
    WasiEnv, WasiError,
};

#[derive(Clone)]
pub struct WasiFunctionEnv {
    pub env: FunctionEnv<WasiEnv>,
}

impl WasiFunctionEnv {
    pub fn new(store: &mut impl AsStoreMut, env: WasiEnv) -> Self {
        Self {
            env: FunctionEnv::new(store, env),
        }
    }

    /// Get an `Imports` for a specific version of WASI detected in the module.
    pub fn import_object(
        &self,
        store: &mut impl AsStoreMut,
        module: &Module,
    ) -> Result<Imports, WasiError> {
        let wasi_version = get_wasi_version(module, false).ok_or(WasiError::UnknownWasiVersion)?;
        Ok(crate::generate_import_object_from_env(
            store,
            &self.env,
            wasi_version,
        ))
    }

    /// Gets a reference to the WasiEnvironment
    pub fn data<'a>(&'a self, store: &'a impl AsStoreRef) -> &'a WasiEnv {
        self.env.as_ref(store)
    }

    /// Gets a mutable- reference to the host state in this context.
    pub fn data_mut<'a>(&'a mut self, store: &'a mut impl AsStoreMut) -> &'a mut WasiEnv {
        self.env.as_mut(store)
    }

    /// Initializes the WasiEnv using the instance exports
    /// (this must be executed before attempting to use it)
    /// (as the stores can not by themselves be passed between threads we can store the module
    ///  in a thread-local variables and use it later - for multithreading)
    pub fn initialize(
        &mut self,
        store: &mut impl AsStoreMut,
        instance: Instance,
    ) -> Result<(), ExportError> {
        self.initialize_with_memory(store, instance, None)
    }

    /// Initializes the WasiEnv using the instance exports and a provided optional memory
    /// (this must be executed before attempting to use it)
    /// (as the stores can not by themselves be passed between threads we can store the module
    ///  in a thread-local variables and use it later - for multithreading)
    pub fn initialize_with_memory(
        &mut self,
        store: &mut impl AsStoreMut,
        instance: Instance,
        memory: Option<Memory>,
    ) -> Result<(), ExportError> {
        // List all the exports and imports
        for ns in instance.module().exports() {
            //trace!("module::export - {} ({:?})", ns.name(), ns.ty());
            trace!("module::export - {}", ns.name());
        }
        for ns in instance.module().imports() {
            trace!("module::import - {}::{}", ns.module(), ns.name());
        }

        let is_wasix_module = crate::utils::is_wasix_module(instance.module());

        // First we get the malloc function which if it exists will be used to
        // create the pthread_self structure
        let memory = instance.exports.get_memory("memory").map_or_else(
            |e| {
                if let Some(memory) = memory {
                    Ok(memory)
                } else {
                    Err(e)
                }
            },
            |v| Ok(v.clone()),
        )?;

        let new_inner = WasiInstanceHandles::new(memory, store, instance);

        let env = self.data_mut(store);
        env.inner.replace(new_inner);

        env.state.fs.set_is_wasix(is_wasix_module);

        // Set the base stack
        let mut stack_base = if let Some(stack_pointer) = env.inner().stack_pointer.clone() {
            match stack_pointer.get(store) {
                wasmer::Value::I32(a) => a as u64,
                wasmer::Value::I64(a) => a as u64,
                _ => 0,
            }
        } else {
            0
        };
        if stack_base == 0 {
            stack_base = DEFAULT_STACK_SIZE;
        }

        // Update the stack layout which is need for asyncify
        let env = self.data_mut(store);
        let layout = &mut env.layout;
        layout.stack_upper = stack_base;
        layout.stack_size = layout.stack_upper - layout.stack_lower;

        Ok(())
    }

    /// Re-initializes this environment using a supplied module and memory
    ///
    /// This must be called after doing an initialize and is normally executed
    /// when the memory and/or module is changed
    pub fn reinitialize(
        &mut self,
        store: &mut Store,
        module: &Module,
        memory: Memory,
    ) -> Result<(), InstantiationError> {
        // Create a new store and copy the memory to it
        let env = self.data(&store).duplicate();
        let memory = {
            let mut new_store = self.data(&store).runtime().new_store();
            let memory = memory.clone_in_store(store, &mut new_store).unwrap();
            std::mem::swap(&mut new_store, store);
            memory
        };

        // Recreate a new store after extracting the environment
        self.env = FunctionEnv::new(store, env);

        // Build the context object and import the memory
        let (mut import_object, init) =
            crate::import_object_for_all_wasi_versions(module, store, &self.env);
        import_object.define("env", "memory", memory.clone());

        let instance = Instance::new(store, module, &import_object)?;
        init(&instance, &store).unwrap();

        // Set the current thread ID
        self.data_mut(store).inner = Some(WasiInstanceHandles::new(memory, &store, instance));
        Ok(())
    }

    /// Like `import_object` but containing all the WASI versions detected in
    /// the module.
    pub fn import_object_for_all_wasi_versions(
        &self,
        store: &mut impl AsStoreMut,
        module: &Module,
    ) -> Result<Imports, WasiError> {
        let wasi_versions =
            get_wasi_versions(module, false).ok_or(WasiError::UnknownWasiVersion)?;

        let mut resolver = Imports::new();
        for version in wasi_versions.iter() {
            let new_import_object =
                crate::generate_import_object_from_env(store, &self.env, *version);
            for ((n, m), e) in new_import_object.into_iter() {
                resolver.define(&n, &m, e);
            }
        }

        Ok(resolver)
    }

    pub fn cleanup(&self, store: &mut impl AsStoreMut, exit_code: Option<ExitCode>) {
        trace!(
            "wasi[{}:{}]::cleanup - destroying local thread variables",
            self.data(store).pid(),
            self.data(store).tid()
        );

        // Destroy all the local thread variables that were allocated for this thread
        let to_local_destroy = {
            let thread_id = self.data(store).thread.tid();
            let mut to_local_destroy = Vec::new();
            let mut inner = self.data(store).process.write();
            for ((thread, key), val) in inner.thread_local.iter() {
                if *thread == thread_id {
                    if let Some(user_data) = inner.thread_local_user_data.get(key) {
                        to_local_destroy.push((*user_data, *val))
                    }
                }
            }
            inner.thread_local.retain(|(t, _), _| *t != thread_id);
            to_local_destroy
        };
        if !to_local_destroy.is_empty() {
            if let Some(thread_local_destroy) = self
                .data(store)
                .inner()
                .thread_local_destroy
                .as_ref()
                .cloned()
            {
                for (user_data, val) in to_local_destroy {
                    let user_data_low: u32 = (user_data & 0xFFFFFFFF) as u32;
                    let user_data_high: u32 = (user_data >> 32) as u32;

                    let val_low: u32 = (val & 0xFFFFFFFF) as u32;
                    let val_high: u32 = (val >> 32) as u32;

                    let _ = thread_local_destroy.call(
                        store,
                        user_data_low as i32,
                        user_data_high as i32,
                        val_low as i32,
                        val_high as i32,
                    );
                }
            }
        }

        // Cleans up all the open files (if this is the main thread)
        self.data(store).blocking_cleanup(exit_code);
    }
}
