export interface EvidenceBadgeState {
  bootId: string;
  scanId: string;
  activeRevision: string;
  outputSeq: string;
  selectedInScan: boolean;
  guardReason: string;
  desiredMask: number;
  appliedMask: number;
  driverApplied: "APPLIED" | "IO_FAULT" | "EXPIRED" | "REJECTED_SEQUENCE" | string;
  physicalObservedMask: number | null;
  physicalObservationSource: string;
  activePersisted: boolean;
  observedAtNs: string;
  expiresAtNs: string;
}

export interface DeviceStateResponse {
  deviceId: string;
  bootId: string;
  scanId: string;
  activeRevision: string;
  controlArmed: boolean;
  fallbackLatched: boolean;
  needsOperatorReview: boolean;
  missedReleases: string;
  activeRuleSet: unknown | null;
  lastEvidence: EvidenceBadgeState | null;
}

export function isDriverAppliedOk(ev: EvidenceBadgeState): boolean {
  return ev.driverApplied === "APPLIED";
}

export function isPhysicalObservedOk(ev: EvidenceBadgeState): boolean {
  return ev.physicalObservedMask !== null && ev.physicalObservedMask !== undefined;
}

export function hasWitnessDiscrepancy(ev: EvidenceBadgeState): boolean {
  return isPhysicalObservedOk(ev) && ev.physicalObservedMask !== (ev.appliedMask & 3);
}

export interface PendingTxRecord {
  txId: string;
  action: "PROPOSE" | "AUTHORIZE" | "ACTIVATE" | "ROLLBACK";
  packageHash?: string;
  createdAtMs: number;
}

/**
 * Pure REST client & reconnect recovery helper for Luckfox PLC Supervisor (:8443) (§6.3 / D02).
 */
export class PlcConsoleClient {
  constructor(
    private readonly baseUrl: string,
    private readonly token: string,
    private readonly fetchFn: typeof fetch = fetch
  ) {}

  private async req<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await this.fetchFn(`${this.baseUrl}${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${this.token}`,
        "Content-Type": "application/json",
      },
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    const text = await res.text();
    const data = text ? JSON.parse(text) : {};
    if (!res.ok) {
      throw new Error(`HTTP ${res.status}: ${data.error ?? text}`);
    }
    return data as T;
  }

  getState(deviceId: string): Promise<DeviceStateResponse> {
    return this.req("GET", `/v1/devices/${encodeURIComponent(deviceId)}/state`);
  }

  getTelemetry(deviceId: string): Promise<{ recentScans: unknown[]; auditLog: unknown[] }> {
    return this.req("GET", `/v1/devices/${encodeURIComponent(deviceId)}/telemetry`);
  }

  getProposal(txId: string): Promise<{ transaction: unknown; activeRevision: string }> {
    return this.req("GET", `/v1/proposals/${encodeURIComponent(txId)}`);
  }

  propose(deviceId: string, txId: string, pkg: unknown, shadowTrace?: unknown[]): Promise<unknown> {
    return this.req("POST", `/v1/devices/${encodeURIComponent(deviceId)}/proposals`, {
      txId,
      package: pkg,
      ...(shadowTrace ? { shadowTrace } : {}),
    });
  }

  authorize(
    txId: string,
    operatorId: string,
    expectedPackageHash: string,
    ttlMs = 60_000
  ): Promise<unknown> {
    return this.req("POST", `/v1/proposals/${encodeURIComponent(txId)}/authorizations`, {
      operatorId,
      expectedPackageHash,
      ttlMs,
    });
  }

  activate(txId: string): Promise<unknown> {
    return this.req("POST", `/v1/proposals/${encodeURIComponent(txId)}/activate`, {});
  }

  cancel(txId: string): Promise<unknown> {
    return this.req("POST", `/v1/proposals/${encodeURIComponent(txId)}/cancel`, {});
  }

  rollback(deviceId: string, txId: string, baseRevision: string, reason: string): Promise<unknown> {
    return this.req("POST", `/v1/devices/${encodeURIComponent(deviceId)}/rollbacks`, {
      txId,
      baseRevision,
      reason,
    });
  }

  manualOverride(deviceId: string, mask: number, validMask: number, ttlMs: number): Promise<unknown> {
    return this.req("POST", `/v1/devices/${encodeURIComponent(deviceId)}/manual`, {
      mask,
      validMask,
      ttlMs,
    });
  }

  rearm(deviceId: string): Promise<{ controlArmed: boolean }> {
    return this.req("POST", `/v1/devices/${encodeURIComponent(deviceId)}/rearm`, {});
  }

  /**
   * Reconciles a pending `txId` after network drop without blindly re-submitting a new txId (D02/V02).
   */
  async reconcilePendingTx(
    deviceId: string,
    pending: PendingTxRecord
  ): Promise<{ phase: "VERIFYING" | "SETTLED" | "NOT_FOUND"; detail: string }> {
    try {
      const prop = await this.getProposal(pending.txId);
      const st = (prop.transaction as { revision?: { state?: string } })?.revision?.state ?? "UNKNOWN";
      const terminalStates = new Set([
        "STABLE",
        "ACTIVE_VOLATILE",
        "REJECTED",
        "SUPERSEDED",
        "ROLLED_BACK",
        "RECOVERY_REQUIRED",
      ]);
      const settled = terminalStates.has(st);
      return {
        phase: settled ? "SETTLED" : "VERIFYING",
        detail: `${settled ? "Reconciliado" : "Verificando"} txId=${pending.txId}: estado=${st}, revActiva=${prop.activeRevision}`,
      };
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (msg.includes("HTTP 404")) {
        const state = await this.getState(deviceId);
        return {
          phase: "NOT_FOUND",
          detail: `txId=${pending.txId} no encontrado; revActiva=${state.activeRevision}`,
        };
      }
      return {
        phase: "VERIFYING",
        detail: `Verificando estado tras reconexion (${msg})`,
      };
    }
  }
}
