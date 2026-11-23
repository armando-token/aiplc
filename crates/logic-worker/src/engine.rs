use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::time::Instant;

use base64_mini::decode_base64;
use control_abi::crypto::{sha256_hex, verify_module_package_signature};
use control_abi::schema::ModulePackageV1;
use control_abi::types::{EvalStatus, ABI_VERSION, MAX_IPC_FRAME_BYTES};
use control_abi::wire::{
    decode_eval_result, encode_eval_request, EvalRequest, EvalResult,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ir_eval::evaluate_ruleset_a0;
use crate::wasm_inspect::{validate_aot_magic, validate_wasm_module_bytes};

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("ABI/Wire error: {0}")]
    Abi(#[from] control_abi::types::AbiError),
    #[error("signature/digest verification failed: {0}")]
    Verification(String),
    #[error("wasm validation failed: {0}")]
    WasmInspect(#[from] crate::wasm_inspect::WasmInspectError),
    #[error("engine runtime error: {0}")]
    Runtime(String),
    #[error("engine not warmed/ready")]
    NotReady,
    #[error("IPC error: {0}")]
    Ipc(String),
    #[error("deadline exceeded")]
    DeadlineExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineKind {
    EmptyNoop,
    NativeIr,
    WamrInterp,
    WamrAot,
    #[serde(rename = "quickjs", alias = "quick-js")]
    QuickJs,
    Wasmtime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineCapabilities {
    pub kind: EngineKind,
    pub uses_aot: bool,
    pub supports_fuel_or_interrupt: bool,
    pub max_linear_mem_bytes: u32,
}

pub trait LogicEngine: Send {
    fn capabilities(&self) -> EngineCapabilities;
    fn prepare(&mut self, pkg: &ModulePackageV1, trusted_pub_hex: &str) -> Result<(), WorkerError>;
    fn warm(&mut self) -> Result<(), WorkerError>;
    fn dispatch(&mut self, req: &EvalRequest) -> Result<(), WorkerError>;
    fn read_result(&mut self) -> Result<EvalResult, WorkerError>;
    fn revoke(&mut self);

    fn eval_sync(&mut self, req: &EvalRequest) -> Result<EvalResult, WorkerError> {
        self.dispatch(req)?;
        self.read_result()
    }
}

// ---------------- Base64 minimal decoder (no extra crate dependency) ----------------
mod base64_mini {
    pub fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(input.len() * 3 / 4);
        let mut buf: u32 = 0;
        let mut bits: u32 = 0;
        for b in input.bytes() {
            if b.is_ascii_whitespace() {
                continue;
            }
            if b == b'=' {
                break;
            }
            let val = match b {
                b'A'..=b'Z' => b - b'A',
                b'a'..=b'z' => b - b'a' + 26,
                b'0'..=b'9' => b - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return Err(format!("invalid base64 byte {b}")),
            };
            buf = (buf << 6) | (val as u32);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((buf >> bits) & 0xff) as u8);
            }
        }
        Ok(out)
    }
}

// ---------------- A0: Empty Noop & Native Rust IR ----------------
pub struct A0NativeIrEngine {
    noop_only: bool,
    pkg: Option<ModulePackageV1>,
    warmed: bool,
    last_result: Option<EvalResult>,
    revoked: bool,
}

impl A0NativeIrEngine {
    pub fn new_ir() -> Self {
        Self {
            noop_only: false,
            pkg: None,
            warmed: false,
            last_result: None,
            revoked: false,
        }
    }

    pub fn new_empty_noop() -> Self {
        Self {
            noop_only: true,
            pkg: None,
            warmed: false,
            last_result: None,
            revoked: false,
        }
    }
}

impl LogicEngine for A0NativeIrEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            kind: if self.noop_only {
                EngineKind::EmptyNoop
            } else {
                EngineKind::NativeIr
            },
            uses_aot: false,
            supports_fuel_or_interrupt: true,
            max_linear_mem_bytes: 0,
        }
    }

    fn prepare(&mut self, pkg: &ModulePackageV1, trusted_pub_hex: &str) -> Result<(), WorkerError> {
        verify_module_package_signature(pkg, trusted_pub_hex)
            .map_err(|e| WorkerError::Verification(e.to_string()))?;
        self.pkg = Some(pkg.clone());
        self.warmed = false;
        self.revoked = false;
        Ok(())
    }

    fn warm(&mut self) -> Result<(), WorkerError> {
        if self.pkg.is_none() {
            return Err(WorkerError::NotReady);
        }
        self.warmed = true;
        Ok(())
    }

    fn dispatch(&mut self, req: &EvalRequest) -> Result<(), WorkerError> {
        if self.revoked || !self.warmed {
            return Err(WorkerError::NotReady);
        }
        let t0 = Instant::now();
        let pkg = self.pkg.as_ref().ok_or(WorkerError::NotReady)?;
        let res = if self.noop_only {
            EvalResult {
                abi_version: ABI_VERSION,
                status: EvalStatus::Ok,
                lease_epoch: req.lease_epoch,
                generation: req.generation,
                scan_id: req.scan_id,
                state_revision: req.state_revision,
                desired_mask: 0,
                valid_mask: 0,
                diag_code: 0,
                eval_duration_ns: t0.elapsed().as_nanos().min(u32::MAX as u128) as u32,
                blocks: req.blocks.clone(),
            }
        } else {
            let mut r = evaluate_ruleset_a0(&pkg.rule_set, req, 0);
            r.eval_duration_ns = t0.elapsed().as_nanos().min(u32::MAX as u128) as u32;
            r
        };
        self.last_result = Some(res);
        Ok(())
    }

    fn read_result(&mut self) -> Result<EvalResult, WorkerError> {
        self.last_result.take().ok_or(WorkerError::NotReady)
    }

    fn revoke(&mut self) {
        self.revoked = true;
        self.last_result = None;
    }
}

// ---------------- C FFI declarations for WAMR & QuickJS-NG ----------------
#[repr(C)]
struct PlcWamrSlotOpaque {
    _unused: [u8; 0],
}
#[repr(C)]
struct PlcQjsSlotOpaque {
    _unused: [u8; 0],
}

extern "C" {
    fn plc_wamr_load_and_warm(
        bytes: *const u8,
        len: u32,
        stack_size: u32,
        heap_size: u32,
        io_buf_cap: u32,
        err_buf: *mut c_char,
        err_cap: u32,
    ) -> *mut PlcWamrSlotOpaque;

    fn plc_wamr_eval(
        slot: *mut PlcWamrSlotOpaque,
        req_bytes: *const u8,
        req_len: u32,
        res_out: *mut u8,
        res_cap: u32,
        err_buf: *mut c_char,
        err_cap: u32,
    ) -> i32;

    fn plc_wamr_terminate(slot: *mut PlcWamrSlotOpaque);
    fn plc_wamr_free(slot: *mut PlcWamrSlotOpaque);

    fn plc_qjs_load_and_warm(
        js_src: *const c_char,
        js_len: usize,
        mem_limit_bytes: usize,
        stack_limit_bytes: usize,
        err_buf: *mut c_char,
        err_cap: u32,
    ) -> *mut PlcQjsSlotOpaque;

    fn plc_qjs_eval(
        slot: *mut PlcQjsSlotOpaque,
        req_bytes: *const u8,
        req_len: u32,
        res_out: *mut u8,
        res_cap: u32,
        budget_ns: u64,
        err_buf: *mut c_char,
        err_cap: u32,
    ) -> i32;

    fn plc_qjs_abort(slot: *mut PlcQjsSlotOpaque);
    fn plc_qjs_free(slot: *mut PlcQjsSlotOpaque);
}

fn c_err_to_string(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

// ---------------- A1: WAMR (Interp & AOT) ----------------
pub struct A1WamrEngine {
    use_aot: bool,
    module_bytes: Option<Vec<u8>>,
    max_linear_mem_bytes: u32,
    slot: *mut PlcWamrSlotOpaque,
    req_buf: Vec<u8>,
    res_buf: Vec<u8>,
    last_result: Option<EvalResult>,
    revoked: bool,
}

unsafe impl Send for A1WamrEngine {}

impl A1WamrEngine {
    pub fn new(use_aot: bool) -> Self {
        Self {
            use_aot,
            module_bytes: None,
            max_linear_mem_bytes: 128 * 1024,
            slot: std::ptr::null_mut(),
            req_buf: Vec::with_capacity(4096),
            res_buf: vec![0u8; 4096],
            last_result: None,
            revoked: false,
        }
    }
}

impl Drop for A1WamrEngine {
    fn drop(&mut self) {
        if !self.slot.is_null() {
            unsafe { plc_wamr_free(self.slot) };
            self.slot = std::ptr::null_mut();
        }
    }
}

impl LogicEngine for A1WamrEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            kind: if self.use_aot {
                EngineKind::WamrAot
            } else {
                EngineKind::WamrInterp
            },
            uses_aot: self.use_aot,
            supports_fuel_or_interrupt: false,
            max_linear_mem_bytes: self.max_linear_mem_bytes,
        }
    }

    fn prepare(&mut self, pkg: &ModulePackageV1, trusted_pub_hex: &str) -> Result<(), WorkerError> {
        verify_module_package_signature(pkg, trusted_pub_hex)
            .map_err(|e| WorkerError::Verification(e.to_string()))?;
        if !pkg.requested_capabilities.is_empty() {
            return Err(WorkerError::Verification(format!(
                "forbidden capabilities requested: {:?}",
                pkg.requested_capabilities
            )));
        }
        let max_mem = pkg.max_linear_mem_bytes.clamp(65536, 512 * 1024);
        self.max_linear_mem_bytes = max_mem;

        // Always validate source Wasm first (both interpreter and AOT)
        let wasm_b64 = pkg.wasm_base64.as_ref().ok_or_else(|| {
            WorkerError::Verification("missing wasmBase64 for WAMR validation".into())
        })?;
        let wasm_bytes = decode_base64(wasm_b64)
            .map_err(|e| WorkerError::Verification(format!("bad wasmBase64: {e}")))?;
        let actual_hash = sha256_hex(&wasm_bytes);
        if pkg.wasm_digest.as_deref() != Some(actual_hash.as_str()) {
            return Err(WorkerError::Verification(
                "wasmDigest mismatch against wasmBase64".into(),
            ));
        }
        validate_wasm_module_bytes(&wasm_bytes, max_mem)?;
        if !self.use_aot {
            self.module_bytes = Some(wasm_bytes);
        }

        if self.use_aot {
            let aot_b64 = pkg
                .aot_base64
                .as_ref()
                .ok_or_else(|| WorkerError::Verification("missing aotBase64 for WAMR AOT".into()))?;
            let aot_bytes = decode_base64(aot_b64)
                .map_err(|e| WorkerError::Verification(format!("bad aotBase64: {e}")))?;
            let actual_aot = sha256_hex(&aot_bytes);
            if pkg.aot_digest.as_deref() != Some(actual_aot.as_str()) {
                return Err(WorkerError::Verification(
                    "aotDigest mismatch against signed package".into(),
                ));
            }
            validate_aot_magic(&aot_bytes)?;
            self.module_bytes = Some(aot_bytes);
        }

        if !self.slot.is_null() {
            unsafe { plc_wamr_free(self.slot) };
            self.slot = std::ptr::null_mut();
        }
        self.revoked = false;
        Ok(())
    }

    fn warm(&mut self) -> Result<(), WorkerError> {
        let bytes = self.module_bytes.as_ref().ok_or(WorkerError::NotReady)?;
        if !self.slot.is_null() {
            unsafe { plc_wamr_free(self.slot) };
            self.slot = std::ptr::null_mut();
        }
        let mut err = [0 as c_char; 256];
        let slot = unsafe {
            plc_wamr_load_and_warm(
                bytes.as_ptr(),
                bytes.len() as u32,
                16 * 1024,
                8 * 1024,
                4096,
                err.as_mut_ptr(),
                err.len() as u32,
            )
        };
        if slot.is_null() {
            return Err(WorkerError::Runtime(c_err_to_string(&err)));
        }
        self.slot = slot;
        Ok(())
    }

    fn dispatch(&mut self, req: &EvalRequest) -> Result<(), WorkerError> {
        if self.revoked || self.slot.is_null() {
            return Err(WorkerError::NotReady);
        }
        let t0 = Instant::now();
        encode_eval_request(req, &mut self.req_buf)?;
        let mut err = [0 as c_char; 256];
        let n = unsafe {
            plc_wamr_eval(
                self.slot,
                self.req_buf.as_ptr(),
                self.req_buf.len() as u32,
                self.res_buf.as_mut_ptr(),
                self.res_buf.len() as u32,
                err.as_mut_ptr(),
                err.len() as u32,
            )
        };
        if n <= 0 {
            return Err(WorkerError::Runtime(c_err_to_string(&err)));
        }
        let mut res = decode_eval_result(&self.res_buf[..n as usize])?;
        if res.eval_duration_ns == 0 {
            res.eval_duration_ns = t0.elapsed().as_nanos().min(u32::MAX as u128) as u32;
        }
        self.last_result = Some(res);
        Ok(())
    }

    fn read_result(&mut self) -> Result<EvalResult, WorkerError> {
        self.last_result.take().ok_or(WorkerError::NotReady)
    }

    fn revoke(&mut self) {
        self.revoked = true;
        self.last_result = None;
        if !self.slot.is_null() {
            unsafe { plc_wamr_terminate(self.slot) };
        }
    }
}

// ---------------- A2: QuickJS-NG ----------------
pub struct A2QuickJsEngine {
    js_cstr: Option<CString>,
    mem_limit_bytes: usize,
    eval_budget_ns: u64,
    slot: *mut PlcQjsSlotOpaque,
    req_buf: Vec<u8>,
    res_buf: Vec<u8>,
    last_result: Option<EvalResult>,
    revoked: bool,
}

unsafe impl Send for A2QuickJsEngine {}

impl A2QuickJsEngine {
    pub fn new(eval_budget_ns: u64) -> Self {
        Self {
            js_cstr: None,
            mem_limit_bytes: 8 * 1024 * 1024,
            eval_budget_ns,
            slot: std::ptr::null_mut(),
            req_buf: Vec::with_capacity(4096),
            res_buf: vec![0u8; MAX_IPC_FRAME_BYTES],
            last_result: None,
            revoked: false,
        }
    }
}

impl Drop for A2QuickJsEngine {
    fn drop(&mut self) {
        if !self.slot.is_null() {
            unsafe { plc_qjs_free(self.slot) };
            self.slot = std::ptr::null_mut();
        }
    }
}

impl LogicEngine for A2QuickJsEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            kind: EngineKind::QuickJs,
            uses_aot: false,
            supports_fuel_or_interrupt: true,
            max_linear_mem_bytes: self.mem_limit_bytes as u32,
        }
    }

    fn prepare(&mut self, pkg: &ModulePackageV1, trusted_pub_hex: &str) -> Result<(), WorkerError> {
        verify_module_package_signature(pkg, trusted_pub_hex)
            .map_err(|e| WorkerError::Verification(e.to_string()))?;
        if !pkg.requested_capabilities.is_empty() {
            return Err(WorkerError::Verification(format!(
                "forbidden capabilities requested: {:?}",
                pkg.requested_capabilities
            )));
        }
        let src = pkg
            .js_source
            .as_deref()
            .ok_or_else(|| WorkerError::Verification("missing jsSource for QuickJS-NG".into()))?;
        if sha256_hex(src.as_bytes()) != pkg.canonical_source_digest {
            return Err(WorkerError::Verification(
                "jsSource sha256 does not match canonicalSourceDigest".into(),
            ));
        }
        let cstr = CString::new(src)
            .map_err(|_| WorkerError::Verification("nul byte in jsSource".into()))?;
        self.js_cstr = Some(cstr);
        if !self.slot.is_null() {
            unsafe { plc_qjs_free(self.slot) };
            self.slot = std::ptr::null_mut();
        }
        self.revoked = false;
        Ok(())
    }

    fn warm(&mut self) -> Result<(), WorkerError> {
        let cstr = self.js_cstr.as_ref().ok_or(WorkerError::NotReady)?;
        if !self.slot.is_null() {
            unsafe { plc_qjs_free(self.slot) };
            self.slot = std::ptr::null_mut();
        }
        let mut err = [0 as c_char; 256];
        let slot = unsafe {
            plc_qjs_load_and_warm(
                cstr.as_ptr(),
                cstr.as_bytes().len(),
                self.mem_limit_bytes,
                256 * 1024,
                err.as_mut_ptr(),
                err.len() as u32,
            )
        };
        if slot.is_null() {
            return Err(WorkerError::Runtime(c_err_to_string(&err)));
        }
        self.slot = slot;
        Ok(())
    }

    fn dispatch(&mut self, req: &EvalRequest) -> Result<(), WorkerError> {
        if self.revoked || self.slot.is_null() {
            return Err(WorkerError::NotReady);
        }
        let t0 = Instant::now();
        encode_eval_request(req, &mut self.req_buf)?;
        let mut err = [0 as c_char; 256];
        let n = unsafe {
            plc_qjs_eval(
                self.slot,
                self.req_buf.as_ptr(),
                self.req_buf.len() as u32,
                self.res_buf.as_mut_ptr(),
                self.res_buf.len() as u32,
                self.eval_budget_ns,
                err.as_mut_ptr(),
                err.len() as u32,
            )
        };
        if n <= 0 {
            let msg = c_err_to_string(&err);
            if msg.contains("interrupted") {
                return Err(WorkerError::DeadlineExceeded);
            }
            return Err(WorkerError::Runtime(msg));
        }
        let mut res = decode_eval_result(&self.res_buf[..n as usize])?;
        if res.eval_duration_ns == 0 {
            res.eval_duration_ns = t0.elapsed().as_nanos().min(u32::MAX as u128) as u32;
        }
        self.last_result = Some(res);
        Ok(())
    }

    fn read_result(&mut self) -> Result<EvalResult, WorkerError> {
        self.last_result.take().ok_or(WorkerError::NotReady)
    }

    fn revoke(&mut self) {
        self.revoked = true;
        self.last_result = None;
        if !self.slot.is_null() {
            unsafe { plc_qjs_abort(self.slot) };
        }
    }
}

// ---------------- A3: Wasmtime (Cranelift / Pulley32 with fuel + epoch) ----------------
#[cfg(feature = "wasmtime-engine")]
pub mod wasmtime_impl {
    use super::*;
    use wasmtime::{Config, Engine, Instance, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc};

    pub struct A3WasmtimeEngine {
        engine: Engine,
        module: Option<Module>,
        store: Option<Store<StoreLimits>>,
        instance: Option<Instance>,
        eval_fn: Option<TypedFunc<(i32, i32, i32, i32), i32>>,
        memory: Option<Memory>,
        max_linear_mem_bytes: u32,
        fuel_budget: u64,
        req_buf: Vec<u8>,
        last_result: Option<EvalResult>,
        revoked: bool,
    }

    impl A3WasmtimeEngine {
        pub fn new(fuel_budget: u64) -> Result<Self, WorkerError> {
            let mut cfg = Config::new();
            cfg.consume_fuel(true);
            cfg.epoch_interruption(true);
            let engine = Engine::new(&cfg).map_err(|e| WorkerError::Runtime(e.to_string()))?;
            Ok(Self {
                engine,
                module: None,
                store: None,
                instance: None,
                eval_fn: None,
                memory: None,
                max_linear_mem_bytes: 128 * 1024,
                fuel_budget,
                req_buf: Vec::with_capacity(4096),
                last_result: None,
                revoked: false,
            })
        }
    }

    impl LogicEngine for A3WasmtimeEngine {
        fn capabilities(&self) -> EngineCapabilities {
            EngineCapabilities {
                kind: EngineKind::Wasmtime,
                uses_aot: false,
                supports_fuel_or_interrupt: true,
                max_linear_mem_bytes: self.max_linear_mem_bytes,
            }
        }

        fn prepare(
            &mut self,
            pkg: &ModulePackageV1,
            trusted_pub_hex: &str,
        ) -> Result<(), WorkerError> {
            verify_module_package_signature(pkg, trusted_pub_hex)
                .map_err(|e| WorkerError::Verification(e.to_string()))?;
            if !pkg.requested_capabilities.is_empty() {
                return Err(WorkerError::Verification(format!(
                    "forbidden capabilities requested: {:?}",
                    pkg.requested_capabilities
                )));
            }
            let wasm_b64 = pkg
                .wasm_base64
                .as_ref()
                .ok_or_else(|| WorkerError::Verification("missing wasmBase64".into()))?;
            let wasm_bytes = decode_base64(wasm_b64)
                .map_err(|e| WorkerError::Verification(format!("bad base64: {e}")))?;
            if pkg.wasm_digest.as_deref() != Some(sha256_hex(&wasm_bytes).as_str()) {
                return Err(WorkerError::Verification("wasmDigest mismatch".into()));
            }
            let max_mem = pkg.max_linear_mem_bytes.clamp(65536, 512 * 1024);
            self.max_linear_mem_bytes = max_mem;
            validate_wasm_module_bytes(&wasm_bytes, max_mem)?;
            let module = Module::new(&self.engine, &wasm_bytes)
                .map_err(|e| WorkerError::Runtime(e.to_string()))?;
            self.module = Some(module);
            self.store = None;
            self.instance = None;
            self.revoked = false;
            Ok(())
        }

        fn warm(&mut self) -> Result<(), WorkerError> {
            let module = self.module.as_ref().ok_or(WorkerError::NotReady)?;
            let limits = StoreLimitsBuilder::new()
                .memory_size(self.max_linear_mem_bytes as usize)
                .build();
            let mut store = Store::new(&self.engine, limits);
            store.limiter(|lim| lim);
            store
                .set_fuel(self.fuel_budget)
                .map_err(|e| WorkerError::Runtime(e.to_string()))?;
            store.set_epoch_deadline(1);
            let instance = Instance::new(&mut store, module, &[])
                .map_err(|e| WorkerError::Runtime(e.to_string()))?;
            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or_else(|| WorkerError::Runtime("missing memory export".into()))?;
            let eval_fn = instance
                .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "eval_scan")
                .map_err(|e| WorkerError::Runtime(e.to_string()))?;
            self.store = Some(store);
            self.instance = Some(instance);
            self.memory = Some(memory);
            self.eval_fn = Some(eval_fn);
            Ok(())
        }

        fn dispatch(&mut self, req: &EvalRequest) -> Result<(), WorkerError> {
            if self.revoked {
                return Err(WorkerError::NotReady);
            }
            let t0 = Instant::now();
            let store = self.store.as_mut().ok_or(WorkerError::NotReady)?;
            let instance = self.instance.ok_or(WorkerError::NotReady)?;
            let mem = self.memory.ok_or(WorkerError::NotReady)?;
            let func = self.eval_fn.as_ref().ok_or(WorkerError::NotReady)?;

            store
                .set_fuel(self.fuel_budget)
                .map_err(|e| WorkerError::Runtime(e.to_string()))?;
            store.set_epoch_deadline(1);
            encode_eval_request(req, &mut self.req_buf)?;

            // Use module-exported static I/O buffers if present, else non-overlapping fallback scratch
            let req_ptr: usize = match instance.get_typed_func::<(), i32>(&mut *store, "get_req_buf") {
                Ok(f) => {
                    let p = f
                        .call(&mut *store, ())
                        .map_err(|e| WorkerError::Runtime(e.to_string()))?;
                    usize::try_from(p)
                        .map_err(|_| WorkerError::Runtime(format!("negative get_req_buf pointer {p}")))?
                }
                Err(_) => 0x4000,
            };
            let res_ptr: usize = match instance.get_typed_func::<(), i32>(&mut *store, "get_res_buf") {
                Ok(f) => {
                    let p = f
                        .call(&mut *store, ())
                        .map_err(|e| WorkerError::Runtime(e.to_string()))?;
                    usize::try_from(p)
                        .map_err(|_| WorkerError::Runtime(format!("negative get_res_buf pointer {p}")))?
                }
                Err(_) => 0xC000,
            };
            let res_cap: usize = 4096;

            mem.write(&mut *store, req_ptr, &self.req_buf)
                .map_err(|e| WorkerError::Runtime(e.to_string()))?;

            let written = func
                .call(
                    &mut *store,
                    (req_ptr as i32, self.req_buf.len() as i32, res_ptr as i32, res_cap as i32),
                )
                .map_err(|e| {
                    let msg = e.to_string();
                    if msg.contains("fuel") || msg.contains("interrupt") {
                        WorkerError::DeadlineExceeded
                    } else {
                        WorkerError::Runtime(msg)
                    }
                })?;

            if written <= 0 || (written as usize) > res_cap {
                return Err(WorkerError::Runtime(format!("bad len {written}")));
            }
            let mut res_buf = vec![0u8; written as usize];
            mem.read(&*store, res_ptr, &mut res_buf)
                .map_err(|e| WorkerError::Runtime(e.to_string()))?;
            let mut res = decode_eval_result(&res_buf)?;
            res.eval_duration_ns = t0.elapsed().as_nanos().min(u32::MAX as u128) as u32;
            self.last_result = Some(res);
            Ok(())
        }

        fn read_result(&mut self) -> Result<EvalResult, WorkerError> {
            self.last_result.take().ok_or(WorkerError::NotReady)
        }

        fn revoke(&mut self) {
            self.revoked = true;
            self.engine.increment_epoch();
            self.last_result = None;
        }
    }
}
