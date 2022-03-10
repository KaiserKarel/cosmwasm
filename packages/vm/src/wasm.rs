use crate::{
    conversion::to_u32,
    environment::Environment,
    memory::{validate_region, Region},
    static_analysis::ExportInfo,
    BackendApi, CommunicationError, CommunicationResult, Querier, Storage, VmError, VmResult,
};
use parity_wasm::elements::FunctionType;
use wasmer::{Array, HostFunction, Instance as WasmerInstance, Memory as WasmerMemory, WasmPtr};
use wasmer::{Function, Val};
use wasmer_middlewares::metering::{get_remaining_points, set_remaining_points, MeteringPoints};

/// Abstracts over different wasm backends, allowing for both Wasmer as well as the Substrate runtime to be used as the actual VM backend.
pub trait WasmVM {
    type ExportInfo: ExportInfo;
    type Memory: Memory;

    fn module(&self) -> &Self::ExportInfo;
    fn memory(&self) -> Self::Memory;
    fn get_gas_left(&self) -> u64;
    fn set_gas_left(&self, new: u64);
    fn call_function(&self, name: &str, args: &[Val]) -> VmResult<Box<[Val]>>;
}

impl WasmVM for WasmerInstance {
    type ExportInfo = wasmer::Module;
    type Memory = WasmerMemory;

    fn module(&self) -> &Self::ExportInfo {
        self.module()
    }

    fn memory(&self) -> Self::Memory {
        let first: Option<WasmerMemory> = self
            .exports
            .iter()
            .memories()
            .next()
            .map(|pair| pair.1.clone());
        // Every contract in CosmWasm must have exactly one exported memory.
        // This is ensured by `check_wasm`/`check_wasm_memories`, which is called for every
        // contract added to the Cache as well as in integration tests.
        // It is possible to bypass this check when using `Instance::from_code` but then you
        // learn the hard way when this panics, or when trying to upload the contract to chain.
        let memory = first.expect("A contract must have exactly one exported memory.");
        memory
    }

    fn get_gas_left(&self) -> u64 {
        match get_remaining_points(self) {
            MeteringPoints::Remaining(count) => count,
            MeteringPoints::Exhausted => 0,
        }
    }

    fn set_gas_left(&self, new: u64) {
        set_remaining_points(self, new);
    }

    fn call_function(&self, name: &str, args: &[Val]) -> VmResult<Box<[Val]>> {
        // Clone function before calling it to avoid dead locks
        let func = self.exports.get_function(name)?.clone();

        func.call(args).map_err(|runtime_err| -> VmError {
            let err: VmError = match get_remaining_points(self) {
                MeteringPoints::Remaining(_) => VmError::from(runtime_err),
                MeteringPoints::Exhausted => VmError::gas_depletion(),
            };
            err
        })
    }
}

pub trait Memory {
    type Pages: Pages;

    fn size(&self) -> Self::Pages;
    fn get_region(&self, ptr: u32) -> CommunicationResult<Region>;
    fn write_region(&self, ptr: u32, data: &[u8]) -> VmResult<()>;
    fn set_region(&self, ptr: u32, data: Region) -> CommunicationResult<()>;
    fn read_region(&self, ptr: u32, max_length: usize) -> VmResult<Vec<u8>>;
    fn maybe_read_region(&self, ptr: u32, max_length: usize) -> VmResult<Option<Vec<u8>>>;
}

pub trait Pages {
    fn inner(&self) -> u32;
}

impl Memory for WasmerMemory {
    type Pages = wasmer::Pages;

    fn size(&self) -> Self::Pages {
        self.size()
    }

    /// maybe_read_region is like read_region, but gracefully handles null pointer (0) by returning None
    /// meant to be used where the argument is optional (like scan)
    #[cfg(feature = "iterator")]
    fn maybe_read_region(&self, ptr: u32, max_length: usize) -> VmResult<Option<Vec<u8>>> {
        if ptr == 0 {
            Ok(None)
        } else {
            self.read_region(ptr, max_length).map(Some)
        }
    }

    fn read_region(&self, ptr: u32, max_length: usize) -> VmResult<Vec<u8>> {
        let region = self.get_region(ptr)?;

        if region.length > to_u32(max_length)? {
            return Err(CommunicationError::region_length_too_big(
                region.length as usize,
                max_length,
            )
            .into());
        }

        match WasmPtr::<u8, Array>::new(region.offset).deref(self, 0, region.length) {
        Some(cells) => {
            // In case you want to do some premature optimization, this shows how to cast a `&'mut [Cell<u8>]` to `&mut [u8]`:
            // https://github.com/wasmerio/wasmer/blob/0.13.1/lib/wasi/src/syscalls/mod.rs#L79-L81
            let len = region.length as usize;
            let mut result = vec![0u8; len];
            for i in 0..len {
                result[i] = cells[i].get();
            }
            Ok(result)
        }
        None => Err(CommunicationError::deref_err(region.offset, format!(
            "Tried to access memory of region {:?} in wasm memory of size {} bytes. This typically happens when the given Region pointer does not point to a proper Region struct.",
            region,
            self.size().bytes().0
        )).into()),
    }
    }

    fn get_region(&self, ptr: u32) -> CommunicationResult<Region> {
        let wptr = WasmPtr::<Region>::new(ptr);
        match wptr.deref(self) {
            Some(cell) => {
                let region = cell.get();
                validate_region(&region)?;
                Ok(region)
            }
            None => Err(CommunicationError::deref_err(
                ptr,
                "Could not dereference this pointer to a Region",
            )),
        }
    }

    fn write_region(&self, ptr: u32, data: &[u8]) -> VmResult<()> {
        let mut region = self.get_region(ptr)?;

        let region_capacity = region.capacity as usize;
        if data.len() > region_capacity {
            return Err(CommunicationError::region_too_small(region_capacity, data.len()).into());
        }
        match WasmPtr::<u8, Array>::new(region.offset).deref(self, 0, region.capacity) {
            Some(cells) => {
                // In case you want to do some premature optimization, this shows how to cast a `&'mut [Cell<u8>]` to `&mut [u8]`:
                // https://github.com/wasmerio/wasmer/blob/0.13.1/lib/wasi/src/syscalls/mod.rs#L79-L81
                for i in 0..data.len() {
                    cells[i].set(data[i])
                }
                region.length = data.len() as u32;
                self.set_region(ptr, region)?;
                Ok(())
            },
            None => Err(CommunicationError::deref_err(region.offset, format!(
                "Tried to access memory of region {:?} in wasm memory of size {} bytes. This typically happens when the given Region pointer does not point to a proper Region struct.",
                region,
                self.size().bytes().0
            )).into()),
        }
    }

    fn set_region(&self, ptr: u32, data: Region) -> CommunicationResult<()> {
        let wptr = WasmPtr::<Region>::new(ptr);

        match wptr.deref(self) {
            Some(cell) => {
                cell.set(data);
                Ok(())
            }
            None => Err(CommunicationError::deref_err(
                ptr,
                "Could not dereference this pointer to a Region",
            )),
        }
    }
}

impl Pages for wasmer::Pages {
    fn inner(&self) -> u32 {
        self.0
    }
}
