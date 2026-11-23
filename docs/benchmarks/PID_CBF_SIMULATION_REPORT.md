# PID & Experimental CBF Simulation Report (§7.3)

## 1. Scope & Plant Model
Implemented in `packages/simulator/src/plant_pid_cbf.ts` (`ThermalPlant`, `PidController`, `ExperimentalCbfFilter`, `runPidAndCbfClosedLoopSimulation`), verified by `packages/simulator/src/simulator.test.ts` (`npm test --workspace=@plc-auto/simulator`).

- **Discrete First-Order Thermal Plant with Dead Time & Saturation**:
  - `dtSec = 0.02` s (`20` ms control period matching PLC nominal scan)
  - `tauSec = 2.5` s, `deadTimeSec = 0.10` s (`5` scan ring buffer)
  - `ambientTempC = 25.0` °C, `heaterGainCPerUnit = 55.0` °C, output clamped `[0, 1]`
- **PID Controller (`PidController`)**:
  - Conditional-integration anti-windup clamping
  - Derivative-on-measurement (no setpoint kick)
  - Bumpless Manual -> Auto mode entry (`setMode` with `setpoint` and `switchToAutoBumpless`): aligns `I = manualOutput - kp * error` and holds integral accumulation on the transition scan (`skipIntegralNextScan`) so the first `step()` evaluates full `P + I + D` with zero discontinuity, then resumes integral accumulation on `k+1`
- **Experimental CBF Safety Filter (`ExperimentalCbfFilter`)**:
  - Forward-invariant barrier `h(T) = T_max - T >= 0` (`T_max = 60.0` °C, `alpha = 0.25`, `uncertaintyMarginC = 0.05` °C)
  - Dead-time-compensated state prediction accounting for buffered heater energy in the transport pipeline
  - Closed-form O(1) projection without external QP solver, guarded by explicit execution budget (`budgetUs = 500` µs -> fallback `uSafe = 0` on overrun or invalid/non-finite sensor reading)

## 2. Measured Closed-Loop Simulation Results
Executed via `runPidAndCbfClosedLoopSimulation()` (`@plc-auto/simulator`):

| Metric | Measured Value | Target / Bound | Result |
| --- | --- | --- | --- |
| Manual -> Auto entry jump across `pid.step()` (`pidBumplessMaxJumpPct`) | `0` % | `< 1.0` % | PASS |
| First subsequent closed-loop AUTO step delta (`\|u[k+1] - u[k]\|`) | `7.9445` % (normal control ramp toward SP = 45.0 °C) | Informational | — |
| Steady-state setpoint tracking error (`pidSettledErrorC`, SP = 45.0 °C) | `0.000201` °C | `< 0.25` °C | PASS |
| Minimum CBF barrier value (`cbfMinBarrierH = 60.0 - T`) under adversarial `u_nom = 1.0` | `0.183839` °C | `>= 0` °C | PASS |
| CBF interventions across 800 steps (`cbfInterventions`) | `718` | `> 0` | PASS |
| Worst-case CBF filter latency (`cbfMaxElapsedUs`) | `195.5` µs (`171–195.5` µs across runs) | `< 500` µs | PASS |

## 3. Architectural Takeaway
1. **PID**: Safe to run directly inside deterministic 20 ms scan logic with conditional-integration anti-windup and true bumpless Manual -> Auto transfer (`0 %` discontinuity on the transition `pid.step()`, followed by smooth closed-loop convergence to `0.000201 °C` steady-state error).
2. **CBF**: Closed-form 1D delay-compensated CBF executes in sub-millisecond bounded time (`max 195.5 us` on host JS, deterministic C/Wasm AOT equivalent is smaller) and preserves `h(T) >= 0` when transport delay is explicitly modeled in the barrier drift term. Any quality/budget violation falls back cleanly to `COMMAND_OFF` (`0`).
