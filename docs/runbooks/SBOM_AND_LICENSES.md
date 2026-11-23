# Software Bill of Materials (SBOM) & License Inventory

All workspace components and dependencies listed below were verified against `Cargo.toml`, `Cargo.lock`, `package.json`, `node_modules/*/package.json`, and `third_party/`.

## 1. Core Embedded Runtimes & Toolchains (Third-Party)

| Component | Version | License | Architecture Targets Built | Role |
| --- | --- | --- | --- | --- |
| **WAMR (`wasm-micro-runtime`)** | `2.4.6` | `Apache-2.0` (`third_party/wamr-2.4.6/LICENSE`) | `darwin-arm64`, `armv7-linux-gnueabihf`, `aarch64-linux-gnu` | Active/Shadow Wasm Interpreter + Fast-Interp + AoT (`libvmlib.a`, zero WASI) |
| **`wamrc` AOT Compiler** | `2.4.6` | `Apache-2.0` | Host `x86_64` via Rosetta 2 | Offline ahead-of-time WebAssembly -> ELF `.aot` compiler for ARMv7 Cortex-A7 / ARM64 |
| **QuickJS-NG** | `v0.18.0` | `MIT` (`third_party/quickjs-ng-v0.18.0/quickjs.h`) | `darwin-arm64`, `armv7-linux-gnueabihf`, `aarch64-linux-gnu` | Engine `A2` JS fallback/comparison (`libquickjs.a`, interrupt handler + mem limit) |
| **Wasmtime / Cranelift** | `wasmtime 27.0.0` (`cranelift-codegen 0.114.0`) | `Apache-2.0 WITH LLVM-exception` | `darwin-arm64`, `aarch64-linux-gnu` (optional feature) | Engine `A3` host/ARM64 comparator (panics on 32-bit `armv7`) |
| **AssemblyScript (`asc`)** | `0.27.37` | `Apache-2.0` | Host Node v22 toolchain | Deterministic `--runtime stub` compiler for `.wasm` modules |

## 2. Rust Workspace Crates (`crates/*`)
Workspace license: `MIT OR Apache-2.0`. Verified `Cargo.lock` dependency versions:

| Crate | Version | Role |
| --- | --- | --- |
| `control-abi` | `0.1.0` | Canonical binary ABI `PLCQ`/`PLCR`, schemas (`site-profile/v1`, `module-package/v1`, `rule-set/v1`), Ed25519 crypto |
| `field-io` | `0.1.0` | Linux GPIO chardev v2 ioctls, Modbus RTU FC03 THT-02 poller & PTY simulator, freshness gate, LCD socket |
| `logic-worker` | `0.1.0` | Isolated worker runtime (`plc-worker`), Seccomp-BPF (ARMv7/aarch64), cgroup v2, `nobody` drop, Wasm validator |
| `supervisor` | `0.1.0` | Deterministic scan loop (`plc-supervisor`), `StateArena`, `ReactiveShield`, CRC32 WAL `DurableJournal`, HTTP API `:8443` |
| `plc-bench` | `0.1.0` | Benchmark & HIL harness producing 7 telemetry artifacts and hardware fault/soak tests |
| `ed25519-dalek` | `2.2.0` | Ed25519 signature verification & signing |
| `sha2` | `0.10.9` | SHA-256 package / canonical JSON digesting |
| `crc32fast` | `1.5.5` | WAL envelope integrity check |
| `libc` | `0.2.191` | POSIX clock, PTY, prctl/seccomp, setuid, GPIO ioctls |
| `serde` / `serde_json` | `1.0.229` / `1.0.154` | Strict JSON wire & WAL serialization |
| `thiserror` | `1.0.69` / `2.0.21` | Typed error definitions |
| `cc` | `1.11.3` (build-dep) | Compiles `csrc/wamr_qjs_shim.c` against `libvmlib.a` / `libquickjs.a` |

## 3. TypeScript Monorepo (`packages/*`) & Toolchain Dependencies
- `typescript`: `5.9.3` (`Apache-2.0`)
- `@types/node`: `22.20.5` (`MIT`)
- Internal workspace packages (`0.1.0`):
  - `@plc-auto/control-ir`
  - `@plc-auto/toolchain`
  - `@plc-auto/simulator`
  - `@plc-auto/intent`
  - `@plc-auto/iec-import`
  - `@plc-auto/console` (builds zero-CDN offline UI in `packages/console/dist-web/`)

## 4. Apple / Swift Package (`apps/ipad`)
- Package: `PlcAutoIpad` (`swift-tools-version: 5.9`, targets `PlcAutoCore`, `PlcAutoUI`, `PlcAutoCoreTests`)
- External Swift dependencies: Zero third-party SPM dependencies (`Foundation`, optional `SwiftUI`/`Speech` system frameworks only).
