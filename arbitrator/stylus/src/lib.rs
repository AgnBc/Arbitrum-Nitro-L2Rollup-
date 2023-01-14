// Copyright 2022-2023, Offchain Labs, Inc.
// For license information, see https://github.com/nitro/blob/master/LICENSE

use env::WasmEnv;
use eyre::ErrReport;
use prover::programs::prelude::*;
use run::{RunProgram, UserOutcome};
use std::mem;
use wasmer::{Bytes, Module};

mod env;
pub mod host;
pub mod run;
pub mod stylus;

#[cfg(test)]
mod test;

#[cfg(all(test, feature = "benchmark"))]
mod benchmarks;

#[derive(PartialEq, Eq)]
#[repr(u8)]
pub enum StylusStatus {
    Success,
    Revert,
    OutOfGas,
    OutOfStack,
    Failure,
}

#[repr(C)]
pub struct GoParams {
    version: u32,
    max_depth: u32,
    heap_bound: u32,
    wasm_gas_price: u64,
    hostio_cost: u64,
}

impl GoParams {
    fn config(self) -> StylusConfig {
        let mut config = StylusConfig::version(self.version);
        config.max_depth = self.max_depth;
        config.heap_bound = Bytes(self.heap_bound as usize);
        config.pricing.wasm_gas_price = self.wasm_gas_price;
        config.pricing.hostio_cost = self.hostio_cost;
        config
    }
}

#[repr(C)]
pub struct GoSlice {
    ptr: *const u8,
    len: usize,
}

impl GoSlice {
    unsafe fn slice(&self) -> &[u8] {
        std::slice::from_raw_parts(self.ptr, self.len)
    }
}

#[repr(C)]
pub struct RustVec {
    ptr: *mut *mut u8,
    len: *mut usize,
    cap: *mut usize,
}

impl RustVec {
    unsafe fn write(&mut self, mut vec: Vec<u8>) {
        let ptr = vec.as_mut_ptr();
        let len = vec.len();
        let cap = vec.capacity();
        mem::forget(vec);
        *self.ptr = ptr;
        *self.len = len;
        *self.cap = cap;
    }

    unsafe fn write_err(&mut self, err: ErrReport) {
        let msg = format!("{:?}", err);
        let vec = msg.into_bytes();
        self.write(vec)
    }
}

#[no_mangle]
pub unsafe extern "C" fn stylus_compile(
    wasm: GoSlice,
    params: GoParams,
    mut output: RustVec,
) -> StylusStatus {
    let wasm = wasm.slice();
    let config = params.config();

    match stylus::module(wasm, config) {
        Ok(module) => {
            output.write(module);
            StylusStatus::Success
        }
        Err(error) => {
            output.write_err(error);
            StylusStatus::Revert
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn stylus_call(
    module: GoSlice,
    calldata: GoSlice,
    params: GoParams,
    mut output: RustVec,
    evm_gas: *mut u64,
) -> StylusStatus {
    use StylusStatus::*;

    let module = module.slice();
    let calldata = calldata.slice();
    let config = params.config();
    let pricing = config.pricing;
    let wasm_gas = pricing.evm_to_wasm(*evm_gas).unwrap_or(u64::MAX);

    macro_rules! error {
        ($msg:expr, $report:expr) => {{
            let report: ErrReport = $report.into();
            let report = report.wrap_err(ErrReport::msg($msg));
            output.write_err(report);
            *evm_gas = 0; // burn all gas
            return Failure;
        }};
    }

    let init = || {
        let env = WasmEnv::new(config.clone(), calldata.to_vec());
        let store = config.store();
        let module = Module::deserialize(&store, module)?;
        stylus::instance_from_module(module, store, env)
    };
    let mut native = match init() {
        Ok(native) => native,
        Err(error) => error!("failed to instantiate program", error),
    };
    native.set_gas(wasm_gas);

    let outcome = match native.run_main(calldata, &config) {
        Ok(outcome) => outcome,
        Err(error) => error!("failed to execute program", error),
    };
    let (status, outs) = match outcome {
        UserOutcome::Success(outs) => (Success, outs),
        UserOutcome::Revert(outs) => (Revert, outs),
        UserOutcome::OutOfGas => (OutOfGas, vec![]),
        UserOutcome::OutOfStack => (OutOfStack, vec![]),
    };

    if pricing.wasm_gas_price != 0 {
        *evm_gas = pricing.wasm_to_evm(wasm_gas);
    }
    if status == OutOfGas {
        *evm_gas = 0;
    }
    output.write(outs);
    status
}

#[no_mangle]
pub unsafe extern "C" fn stylus_free(vec: RustVec) {
    let vec = Vec::from_raw_parts(*vec.ptr, *vec.len, *vec.cap);
    mem::drop(vec)
}