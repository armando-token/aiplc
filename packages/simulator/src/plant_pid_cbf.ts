/**
 * Continuous Thermal Plant (1st-order + delay + saturation),
 * Discrete PID Controller (anti-windup + derivative-on-measurement + bumpless Auto/Manual transfer),
 * and Experimental Control Barrier Function (CBF) Safety Filter (§7.3).
 */

export interface ThermalPlantConfig {
  dtSec: number;
  tauSec: number;
  ambientTempC: number;
  heaterGainCPerUnit: number;
  coolingGainCPerUnit: number;
  deadTimeSec: number;
  minTempC: number;
  maxTempC: number;
}

export class ThermalPlant {
  tempC: number;
  private delayBuffer: number[];
  private delayIdx = 0;

  constructor(
    readonly config: ThermalPlantConfig,
    initialTempC = 28.0
  ) {
    this.tempC = initialTempC;
    const delaySteps = Math.max(1, Math.round(config.deadTimeSec / config.dtSec));
    this.delayBuffer = new Array<number>(delaySteps).fill(0);
  }

  /**
   * Advances continuous thermal plant by `dtSec`.
   * @param heaterCmd normalized [0, 1] heater drive (or output actuation)
   * @param coolerCmd normalized [0, 1] cooling fan drive
   */
  step(heaterCmd: number, coolerCmd = 0): number {
    const uHeat = Math.max(0, Math.min(1, heaterCmd));
    const uCool = Math.max(0, Math.min(1, coolerCmd));

    const delayedHeat = this.delayBuffer[this.delayIdx] ?? 0;
    this.delayBuffer[this.delayIdx] = uHeat;
    this.delayIdx = (this.delayIdx + 1) % this.delayBuffer.length;

    const { dtSec, tauSec, ambientTempC, heaterGainCPerUnit, coolingGainCPerUnit, minTempC, maxTempC } =
      this.config;

    const eqTemp = ambientTempC + heaterGainCPerUnit * delayedHeat - coolingGainCPerUnit * uCool;
    this.tempC += (dtSec / tauSec) * (eqTemp - this.tempC);
    this.tempC = Math.max(minTempC, Math.min(maxTempC, this.tempC));
    return this.tempC;
  }
}

export interface PidConfig {
  kp: number;
  ki: number;
  kd: number;
  dtSec: number;
  outMin: number;
  outMax: number;
}

export type PidMode = "MANUAL" | "AUTO";

export class PidController {
  private integral = 0;
  private prevMeas: number | null = null;
  private lastOut = 0;
  private mode: PidMode = "MANUAL";
  private skipIntegralNextScan = false;

  constructor(readonly cfg: PidConfig) {}

  setMode(
    nextMode: PidMode,
    manualOutput = this.lastOut,
    currentMeas?: number,
    setpoint?: number
  ): void {
    const clampedMan = Math.max(this.cfg.outMin, Math.min(this.cfg.outMax, manualOutput));
    if (nextMode === "AUTO" && this.mode === "MANUAL") {
      // Bumpless Auto <- Manual initialization:
      // Subtract current proportional term when setpoint & PV are known and hold integral on the transition scan.
      if (setpoint !== undefined && currentMeas !== undefined) {
        this.integral = clampedMan - this.cfg.kp * (setpoint - currentMeas);
      } else {
        this.integral = clampedMan;
      }
      if (currentMeas !== undefined) {
        this.prevMeas = currentMeas;
      }
      this.skipIntegralNextScan = true;
    } else if (nextMode === "MANUAL") {
      this.skipIntegralNextScan = false;
    }
    this.mode = nextMode;
    this.lastOut = clampedMan;
  }

  getMode(): PidMode {
    return this.mode;
  }

  /**
   * Computes discrete PID output with:
   * - Proportional on error during AUTO, bumpless offset initialization on transition
   * - Derivative-on-measurement (no setpoint derivative kick)
   * - Conditional integration clamping anti-windup
   */
  step(setpoint: number, measurement: number, manualOutput = this.lastOut): number {
    const { kp, ki, kd, dtSec, outMin, outMax } = this.cfg;
    if (this.mode === "MANUAL") {
      this.prevMeas = measurement;
      this.skipIntegralNextScan = false;
      const clamped = Math.max(outMin, Math.min(outMax, manualOutput));
      this.integral = clamped - kp * (setpoint - measurement);
      this.lastOut = clamped;
      return clamped;
    }

    const error = setpoint - measurement;
    const pTerm = kp * error;
    const dMeas = this.prevMeas === null ? 0 : (measurement - this.prevMeas) / dtSec;
    const dTerm = -kd * dMeas;
    this.prevMeas = measurement;

    const integrate = !this.skipIntegralNextScan;
    this.skipIntegralNextScan = false;
    const candidateI = integrate ? this.integral + ki * error * dtSec : this.integral;
    const raw = pTerm + candidateI + dTerm;
    const clamped = Math.max(outMin, Math.min(outMax, raw));

    // Anti-windup clamping: do not integrate further into saturation
    const saturatedHigh = raw > outMax && error > 0;
    const saturatedLow = raw < outMin && error < 0;
    if (!saturatedHigh && !saturatedLow) {
      this.integral = candidateI;
    }

    this.lastOut = clamped;
    return clamped;
  }

  /**
   * Explicit bumpless Manual -> Auto helper keeping exact continuous controller output on first Auto step.
   */
  switchToAutoBumpless(setpoint: number, measurement: number, currentManualOut: number): number {
    const clampedMan = Math.max(this.cfg.outMin, Math.min(this.cfg.outMax, currentManualOut));
    const pTerm = this.cfg.kp * (setpoint - measurement);
    // Align I such that P + I == currentManualOut on the switch scan (zero step bump)
    this.integral = clampedMan - pTerm;
    this.prevMeas = measurement;
    this.mode = "AUTO";
    this.skipIntegralNextScan = true;
    this.lastOut = clampedMan;
    return clampedMan;
  }
}

export interface CbfConfig {
  tMaxC: number;
  alpha: number; // Discrete CBF decay factor in (0, 1]
  tauSec: number;
  dtSec: number;
  ambientTempC: number;
  heaterGainCPerUnit: number;
  uncertaintyMarginC: number;
  budgetUs: number;
}

export interface CbfDecision {
  uSafe: number;
  barrierValue: number;
  filtered: boolean;
  fallbackUsed: boolean;
  elapsedUs: number;
}

/**
 * Experimental Discrete-Time Control Barrier Function (CBF) safety filter (§7.3).
 * Ensures h(T) = T_max - T >= 0 remains forward-invariant under nominal control u_nom
 * without heavy solver dependencies, enforcing an explicit execution budget.
 */
export class ExperimentalCbfFilter {
  private lastU = 0;

  constructor(readonly cfg: CbfConfig) {}

  filter(tempC: number, tempValid: boolean, uNom: number): CbfDecision {
    const startNs = process.hrtime.bigint();
    if (!tempValid || !Number.isFinite(tempC)) {
      const elapsedUs = Number(process.hrtime.bigint() - startNs) / 1000;
      this.lastU = 0;
      return {
        uSafe: 0,
        barrierValue: this.cfg.tMaxC - tempC,
        filtered: true,
        fallbackUsed: true,
        elapsedUs,
      };
    }

    // Dead-time-compensated barrier prediction (~0.1s transport buffer conservatively accounted)
    const kFactor = this.cfg.dtSec / this.cfg.tauSec;
    const delayBufferSteps = 6;
    const delayDriftC =
      delayBufferSteps * kFactor * Math.max(0, this.cfg.heaterGainCPerUnit * this.lastU + this.cfg.ambientTempC - tempC);
    const predTempC = tempC + delayDriftC;

    const h = this.cfg.tMaxC - tempC;
    const hPred = Math.max(0, this.cfg.tMaxC - predTempC);
    const driftNextTemp =
      predTempC + kFactor * (this.cfg.ambientTempC - predTempC) + this.cfg.uncertaintyMarginC;
    const maxNextTemp = this.cfg.tMaxC - (1 - this.cfg.alpha) * hPred;
    const maxAllowedHeat =
      kFactor > 0 && this.cfg.heaterGainCPerUnit > 0
        ? (maxNextTemp - driftNextTemp) / (kFactor * this.cfg.heaterGainCPerUnit)
        : 0;

    const uClamped = Math.max(0, Math.min(1, uNom));
    const uUpper = Math.max(0, Math.min(1, maxAllowedHeat));
    let uSafe = Math.min(uClamped, uUpper);
    let fallbackUsed = false;

    const elapsedUs = Number(process.hrtime.bigint() - startNs) / 1000;
    if (elapsedUs > this.cfg.budgetUs || h <= 0) {
      uSafe = 0;
      fallbackUsed = true;
    }
    this.lastU = uSafe;

    return {
      uSafe,
      barrierValue: h,
      filtered: uSafe < uClamped || fallbackUsed,
      fallbackUsed,
      elapsedUs,
    };
  }
}

export interface SimulationSummary {
  pidBumplessMaxJumpPct: number;
  pidSettledErrorC: number;
  cbfMinBarrierH: number;
  cbfInterventions: number;
  cbfMaxElapsedUs: number;
}

export function runPidAndCbfClosedLoopSimulation(): SimulationSummary {
  const plantCfg: ThermalPlantConfig = {
    dtSec: 0.02,
    tauSec: 2.5,
    ambientTempC: 25.0,
    heaterGainCPerUnit: 55.0,
    coolingGainCPerUnit: 0.0,
    deadTimeSec: 0.1,
    minTempC: 0.0,
    maxTempC: 120.0,
  };

  // Part 1: PID anti-windup + bumpless Auto/Manual test
  const plantPid = new ThermalPlant(plantCfg, 30.0);
  const pid = new PidController({
    kp: 0.12,
    ki: 0.05,
    kd: 0.02,
    dtSec: 0.02,
    outMin: 0.0,
    outMax: 1.0,
  });

  pid.setMode("MANUAL", 0.35, plantPid.tempC);
  let lastManOut = 0.35;
  for (let i = 0; i < 50; i++) {
    lastManOut = pid.step(45.0, plantPid.tempC, 0.35);
    plantPid.step(lastManOut);
  }
  pid.switchToAutoBumpless(45.0, plantPid.tempC, lastManOut);
  const firstAutoOut = pid.step(45.0, plantPid.tempC);
  const pidBumplessMaxJumpPct = Math.abs(firstAutoOut - lastManOut) * 100;
  plantPid.step(firstAutoOut);

  for (let i = 0; i < 1200; i++) {
    const u = pid.step(45.0, plantPid.tempC);
    plantPid.step(u);
  }
  const pidSettledErrorC = Math.abs(45.0 - plantPid.tempC);

  // Part 2: CBF safety supervisor guarding unsafe aggressive command (u_nom = 1.0 trying to reach ~80C)
  const plantCbf = new ThermalPlant(plantCfg, 40.0);
  const cbf = new ExperimentalCbfFilter({
    tMaxC: 60.0,
    alpha: 0.25,
    tauSec: plantCfg.tauSec,
    dtSec: plantCfg.dtSec,
    ambientTempC: plantCfg.ambientTempC,
    heaterGainCPerUnit: plantCfg.heaterGainCPerUnit,
    uncertaintyMarginC: 0.05,
    budgetUs: 500,
  });

  let minH = Infinity;
  let interventions = 0;
  let maxElapsedUs = 0;

  for (let i = 0; i < 800; i++) {
    const dec = cbf.filter(plantCbf.tempC, true, 1.0);
    if (dec.barrierValue < minH) minH = dec.barrierValue;
    if (dec.filtered) interventions++;
    if (dec.elapsedUs > maxElapsedUs) maxElapsedUs = dec.elapsedUs;
    plantCbf.step(dec.uSafe);
  }

  return {
    pidBumplessMaxJumpPct,
    pidSettledErrorC,
    cbfMinBarrierH: minH,
    cbfInterventions: interventions,
    cbfMaxElapsedUs: maxElapsedUs,
  };
}
