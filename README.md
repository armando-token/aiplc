# aiplc — Deterministic Industrial PLC Runtime, Toolchain & HIL Suite (Luckfox Lyra ARMv7 / ARM64)

Sistema determinista para operación y actualización en caliente de lógica de control industrial sobre **Luckfox Lyra PLC** (Rockchip RK3506 `armv7l`, Linux `6.1.99`) y arquitecturas `ARM64` (`aarch64-linux-gnu` / `darwin-arm64`).

## Arquitectura del Monorepo

- **`crates/control-abi`**: Contrato binario canónico `PLCQ`/`PLCR` (`72` B / `56` B + señales de `16` B y bloques de `40` B), esquemas estrictos (`site-profile/v1`, `module-package/v1`, `rule-set/v1`) y verificación criptográfica Ed25519 estricta (`verify_strict`).
- **`crates/field-io`**: Controlador Linux GPIO chardev v2 (`/dev/gpiochip0` líneas `17` y `18`, exclusividad `EBUSY` y safe-off), lector Modbus RTU FC03 + simulador PTY para THT-02 y compuerta de frescura (`1500 ms` TTL).
- **`crates/logic-worker`**: Runtime aislado para módulos de lógica (`A0` IR nativo, **`A1-aot` WAMR 2.4.6 AoT**, `A1-interp` WAMR, `A2` QuickJS-NG `v0.18.0` y `A3` Wasmtime 27.0.0 en 64 bits), con cgroup v2, cierre de FDs, descenso a `UID 65534 nobody`, `NO_NEW_PRIVS` y Seccomp-BPF (`AUDIT_ARCH_ARM` / `AUDIT_ARCH_AARCH64`).
- **`crates/supervisor`**: Lazo determinista de `20 ms`, `ReactiveShield` (interlocks pre y post-dwell), `StateArena` con migración `PRESERVE_ELAPSED`/`RESET`, journal WAL CRC32 resiliente a `SIGKILL` (`DurableJournal`) y servidor API REST + consola offline (`:8443`).
- **`crates/plc-bench`**: Harness de jitter (`SCHED_FIFO`), benchmarks comparativos `W0–W5`, suite HIL en hardware real (`H01`, `H02`, `F01–F06`, `D03`, `S03-A2`) y soak acelerado de `1,000` swaps en eMMC.
- **`packages/*`**: Monorepo TypeScript con `@plc-auto/control-ir`, `@plc-auto/toolchain` (`AssemblyScript -> Wasm -> WAMR AoT` + `QuickJS-NG`), `@plc-auto/simulator` (trazas §8.2, PID bumpless/anti-windup y filtro experimental CBF), `@plc-auto/intent` (motor determinista en español `es-PE`, 120 casos, 0 falsos aceptados), `@plc-auto/iec-import` (ST y PLCopen XML LD -> Ladder SVG) y `@plc-auto/console` (UI offline zero-CDN).
- **`apps/ipad`**: Paquete Swift (`PlcAutoCore`, `PlcAutoUI`, `PlcAutoCoreTests`) con cliente REST idempotente (`txId`), almacén atómico de transacciones pendientes, prefase en español y decodificación exacta de `EvidenceBundle`.

## Seguridad de Llaves de Banco de Pruebas

Los archivos bajo `profiles/demo-lyra/keys/test-bench.*` son llaves sintéticas locales usadas exclusivamente para firmar paquetes de demostración y benchmarks reproducibles (`M1`–`W5`) sin valor de producción.

## Documentación y Evidencia Técnica

- [ENGINE_SELECTION_RECORD.md](docs/benchmarks/ENGINE_SELECTION_RECORD.md)
- [PID_CBF_SIMULATION_REPORT.md](docs/benchmarks/PID_CBF_SIMULATION_REPORT.md)
- [TEST_MATRIX_REPORT.md](docs/verification/TEST_MATRIX_REPORT.md)
- [OPERATION_AND_RECOVERY.md](docs/runbooks/OPERATION_AND_RECOVERY.md)
- [SBOM_AND_LICENSES.md](docs/runbooks/SBOM_AND_LICENSES.md)
