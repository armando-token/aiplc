use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use control_abi::schema::{DeploymentState, ModulePackageV1};
use control_abi::types::InputSignal;
use field_io::FieldIoDriver;
use logic_worker::engine::{A0NativeIrEngine, A1WamrEngine, A2QuickJsEngine, LogicEngine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::clock::ControlClock;
use crate::deploy::{DeployError, ShadowReport};
use crate::scan_loop::ScanSupervisor;
use crate::shield::ManualLease;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditLogEntry {
    pub at_ns: String,
    pub boot_id: String,
    pub actor: String,
    pub role: String,
    pub action: String,
    pub tx_id: Option<String>,
    pub package_hash: Option<String>,
    pub revision: String,
    pub outcome: String,
}

#[derive(Debug, Clone)]
pub struct AuthTokens {
    pub reader: String,
    pub author: String,
    pub operator: String,
    pub engineer: String,
}

impl Default for AuthTokens {
    fn default() -> Self {
        Self {
            reader: "plc-reader-token".into(),
            author: "plc-author-token".into(),
            operator: "plc-operator-token".into(),
            engineer: "plc-engineer-token".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Reader = 1,
    Author = 2,
    Operator = 3,
    Engineer = 4,
}

impl AuthTokens {
    pub fn authenticate(&self, header_val: Option<&str>) -> Option<(String, Role)> {
        let token = header_val
            .and_then(|h| h.strip_prefix("Bearer ").or(Some(h)))
            .map(str::trim)?;
        if token == self.engineer {
            Some(("engineer-1".into(), Role::Engineer))
        } else if token == self.operator {
            Some(("operator-1".into(), Role::Operator))
        } else if token == self.author {
            Some(("author-1".into(), Role::Author))
        } else if token == self.reader {
            Some(("reader-1".into(), Role::Reader))
        } else {
            None
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProposalRequestBody {
    tx_id: String,
    package: ModulePackageV1,
    #[serde(default)]
    shadow_trace: Option<Vec<(u64, Vec<InputSignal>)>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthRequestBody {
    operator_id: String,
    expected_package_hash: String,
    #[serde(default = "default_auth_ttl")]
    ttl_ms: u64,
}
fn default_auth_ttl() -> u64 {
    60_000
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollbackRequestBody {
    tx_id: String,
    base_revision: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManualLeaseBody {
    mask: u32,
    valid_mask: u32,
    ttl_ms: u64,
}

pub struct ApiContext<C: ControlClock, D: FieldIoDriver> {
    pub supervisor: Mutex<ScanSupervisor<C, D>>,
    pub tokens: AuthTokens,
    pub audit_log: Mutex<VecDeque<AuditLogEntry>>,
    pub console_dir: Option<PathBuf>,
}

impl<C: ControlClock, D: FieldIoDriver> ApiContext<C, D> {
    pub fn new(
        supervisor: ScanSupervisor<C, D>,
        tokens: AuthTokens,
        console_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            supervisor: Mutex::new(supervisor),
            tokens,
            audit_log: Mutex::new(VecDeque::with_capacity(512)),
            console_dir,
        }
    }

    fn record_audit(
        &self,
        at_ns: u64,
        boot_id: u64,
        actor: &str,
        role: &str,
        action: &str,
        tx_id: Option<&str>,
        package_hash: Option<&str>,
        revision: u64,
        outcome: &str,
    ) {
        let mut log = self.audit_log.lock().unwrap();
        if log.len() >= 512 {
            log.pop_front();
        }
        log.push_back(AuditLogEntry {
            at_ns: at_ns.to_string(),
            boot_id: boot_id.to_string(),
            actor: actor.to_string(),
            role: role.to_string(),
            action: action.to_string(),
            tx_id: tx_id.map(str::to_string),
            package_hash: package_hash.map(str::to_string),
            revision: revision.to_string(),
            outcome: outcome.to_string(),
        });
    }

    pub fn handle_http_request(
        &self,
        method: &str,
        path: &str,
        auth_header: Option<&str>,
        body: &[u8],
    ) -> (u16, &'static str, Vec<u8>) {
        // Serve static console offline UI if requested (strict canonical containment check)
        if method == "GET" && (path == "/" || path == "/console" || path.starts_with("/console/")) {
            if let Some(ref dir) = self.console_dir {
                let rel = if path == "/" || path == "/console" || path == "/console/" {
                    "index.html"
                } else {
                    path.trim_start_matches("/console").trim_start_matches('/')
                };
                if !rel.is_empty() && !rel.contains("..") && !rel.starts_with('/') {
                    if let (Ok(base_canon), Ok(target_canon)) =
                        (std::fs::canonicalize(dir), std::fs::canonicalize(dir.join(rel)))
                    {
                        if target_canon.starts_with(&base_canon) {
                            if let Ok(bytes) = std::fs::read(&target_canon) {
                                let ctype = if rel.ends_with(".js") {
                                    "application/javascript; charset=utf-8"
                                } else if rel.ends_with(".css") {
                                    "text/css; charset=utf-8"
                                } else {
                                    "text/html; charset=utf-8"
                                };
                                return (200, ctype, bytes);
                            }
                        }
                    }
                }
            }
            return json_resp(
                200,
                &json!({"status": "ok", "service": "plc-auto-supervisor", "consoleBuilt": false}),
            );
        }

        if method == "GET" && path == "/health" {
            return json_resp(200, &json!({"ok": true}));
        }

        // Authenticate all /v1 routes
        let (actor, role) = match self.tokens.authenticate(auth_header) {
            Some(v) => v,
            None => {
                return json_resp(401, &json!({"error": "UNAUTHORIZED", "message": "missing or invalid Bearer token"}));
            }
        };

        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if parts.is_empty() || parts[0] != "v1" {
            return json_resp(404, &json!({"error": "NOT_FOUND"}));
        }

        // GET /v1/devices/{id}/state
        if method == "GET" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "state" {
            let sup = self.supervisor.lock().unwrap();
            if parts[2] != sup.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let health = sup.driver.health();
            let active_hash = sup.deploy.active_package_hash.clone();
            let logic_hash = sup
                .deploy
                .active_package
                .as_ref()
                .and_then(|p| p.wasm_digest.clone().or_else(|| Some(p.ir_digest.clone())));
            let temp = sup.last_temp_signal();
            let resp = json!({
                "deviceId": sup.profile.device_id,
                "bootId": sup.boot_id.to_string(),
                "scanId": sup.scan_id.to_string(),
                "activeRevision": sup.deploy.active_revision.to_string(),
                "packageHash": active_hash,
                "logicHash": logic_hash,
                "profileHash": sup.deploy.profile_hash_str,
                "controlArmed": sup.control_armed,
                "fallbackLatched": sup.deploy.fallback_latched,
                "needsOperatorReview": sup.deploy.needs_operator_review,
                "ioHealthy": health.healthy,
                "driverStatus": format!("{:?}", health.last_status),
                "lastSignal": temp,
                "lastEvidence": sup.last_evidence,
                "activeRuleSet": sup.deploy.active_package.as_ref().map(|p| &p.rule_set),
                "channels": sup.profile.channels,
                "missedReleases": sup.missed_releases.to_string(),
            });
            return json_resp(200, &resp);
        }

        // GET /v1/devices/{id}/telemetry
        if method == "GET" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "telemetry" {
            let sup = self.supervisor.lock().unwrap();
            if parts[2] != sup.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let recent: Vec<Value> = sup
                .recent_scans
                .iter()
                .rev()
                .take(40)
                .map(|r| {
                    json!({
                        "bootId": r.boot_id.to_string(),
                        "scanId": r.scan_id.to_string(),
                        "releaseNs": r.release_ns.to_string(),
                        "activeRev": r.active_rev.to_string(),
                        "desiredMask": r.desired_mask,
                        "appliedMask": r.applied_mask,
                        "observedMask": r.observed_mask,
                        "fallbackReason": r.fallback_reason,
                        "deadlineMiss": r.deadline_miss,
                        "signals": r.signals,
                    })
                })
                .collect();
            let audit: Vec<AuditLogEntry> = self.audit_log.lock().unwrap().iter().cloned().collect();
            return json_resp(
                200,
                &json!({
                    "deviceId": sup.profile.device_id,
                    "bootId": sup.boot_id.to_string(),
                    "lastEvidence": sup.last_evidence,
                    "recentScans": recent,
                    "auditLog": audit,
                }),
            );
        }

        // POST /v1/devices/{id}/proposals
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "proposals" {
            if role < Role::Author {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let parsed: ProposalRequestBody = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "message": e.to_string()})),
            };
            let mut sup = self.supervisor.lock().unwrap();
            if parts[2] != sup.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let now_ns = sup.clock.now_ns();
            let arena_snap = sup.arena.clone();
            let tx_res = sup
                .deploy
                .stage_and_verify(parsed.tx_id.clone(), parsed.package.clone(), &arena_snap);
            let (pkg_hash, rev_num, already_shadowed) = match tx_res {
                Ok(tx) => (
                    tx.revision.package_hash.clone(),
                    tx.revision.revision,
                    tx.shadow_report.is_some(),
                ),
                Err(DeployError::RevisionConflict { expected, got }) => {
                    return json_resp(
                        409,
                        &json!({"error": "BASE_REVISION_CONFLICT", "expected": expected.to_string(), "got": got.to_string()}),
                    );
                }
                Err(DeployError::IdempotencyMismatch(tx_id)) => {
                    return json_resp(
                        409,
                        &json!({"error": "IDEMPOTENCY_CONFLICT", "txId": tx_id}),
                    );
                }
                Err(e) => {
                    return json_resp(422, &json!({"error": "VALIDATION_FAILED", "message": e.to_string()}));
                }
            };

            if !already_shadowed {
                let trace = parsed.shadow_trace.unwrap_or_else(|| {
                    vec![
                        (0, vec![InputSignal { id: 1, value: 449, quality: control_abi::SignalQuality::Good, sample_age_ms: 20 }]),
                        (1_000_000_000, vec![InputSignal { id: 1, value: 451, quality: control_abi::SignalQuality::Good, sample_age_ms: 20 }]),
                        (11_000_000_000, vec![InputSignal { id: 1, value: 451, quality: control_abi::SignalQuality::Good, sample_age_ms: 20 }]),
                    ]
                });
                let mut shadow_eng: Box<dyn LogicEngine> = match parsed.package.engine.as_str() {
                    "wamr-aot" => Box::new(A1WamrEngine::new(true)),
                    "wamr-interp" => Box::new(A1WamrEngine::new(false)),
                    "quickjs" => Box::new(A2QuickJsEngine::new(5_000_000)),
                    _ => Box::new(A0NativeIrEngine::new_ir()),
                };
                let report: ShadowReport = sup.run_shadow_replay(&mut *shadow_eng, &parsed.package, &trace);
                let _ = sup.deploy.record_shadow_result(&parsed.tx_id, report);
            }

            let tx = sup.deploy.transactions.get(&parsed.tx_id).unwrap().clone();
            let boot_id = sup.boot_id;
            drop(sup);
            self.record_audit(
                now_ns,
                boot_id,
                &actor,
                &format!("{role:?}"),
                "PROPOSE_STAGE_SHADOW",
                Some(&parsed.tx_id),
                Some(&pkg_hash),
                rev_num,
                &format!("{:?}", tx.revision.state),
            );
            return json_resp(201, &json!(tx));
        }

        // POST /v1/proposals/{tx}/authorizations
        if method == "POST"
            && parts.len() == 4
            && parts[1] == "proposals"
            && parts[3] == "authorizations"
        {
            if role < Role::Operator {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let body_json: AuthRequestBody = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "message": e.to_string()})),
            };
            let tx_id = parts[2];
            let mut sup = self.supervisor.lock().unwrap();
            // Check role requirement for new code/Wasm changes
            if let Some(tx) = sup.deploy.transactions.get(tx_id) {
                if tx.package.wasm_base64.is_some() && role < Role::Operator {
                    return json_resp(403, &json!({"error": "ROLE_INSUFFICIENT"}));
                }
            }
            let now_ns = sup.clock.now_ns();
            let boot_id = sup.boot_id;
            match sup.deploy.authorize(
                tx_id,
                body_json.operator_id,
                format!("{role:?}"),
                &body_json.expected_package_hash,
                now_ns,
                body_json.ttl_ms,
            ) {
                Ok(tx) => {
                    let tx_clone = tx.clone();
                    drop(sup);
                    self.record_audit(
                        now_ns,
                        boot_id,
                        &actor,
                        &format!("{role:?}"),
                        "AUTHORIZE",
                        Some(tx_id),
                        Some(&tx_clone.revision.package_hash),
                        tx_clone.revision.revision,
                        "ARMED",
                    );
                    return json_resp(200, &json!(tx_clone));
                }
                Err(DeployError::UnknownTx(_)) => return json_resp(404, &json!({"error": "UNKNOWN_TX"})),
                Err(DeployError::RevisionConflict { expected, got }) => {
                    return json_resp(409, &json!({"error": "BASE_REVISION_CONFLICT", "expected": expected.to_string(), "got": got.to_string()}));
                }
                Err(DeployError::AlreadyArmed(other)) => {
                    return json_resp(409, &json!({"error": "ANOTHER_TX_ARMED", "armedTxId": other}));
                }
                Err(e) => return json_resp(422, &json!({"error": "AUTHORIZATION_REJECTED", "message": e.to_string()})),
            }
        }

        // POST /v1/proposals/{tx}/activate
        if method == "POST"
            && parts.len() == 4
            && parts[1] == "proposals"
            && parts[3] == "activate"
        {
            if role < Role::Operator {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let tx_id = parts[2];
            let mut sup = self.supervisor.lock().unwrap();
            let already_active = sup.deploy.transactions.get(tx_id).is_some_and(|t| {
                matches!(
                    t.revision.state,
                    DeploymentState::ActiveVolatile | DeploymentState::Stable
                )
            });
            if already_active {
                let tx = sup.deploy.transactions.get(tx_id).unwrap().clone();
                return json_resp(200, &json!(tx));
            }
            if !sup.control_armed || sup.deploy.fallback_latched {
                return json_resp(
                    409,
                    &json!({
                        "error": "CONTROL_DISARMED_OR_LATCHED",
                        "controlArmed": sup.control_armed,
                        "fallbackLatched": sup.deploy.fallback_latched
                    }),
                );
            }
            let now_ns = sup.clock.now_ns();
            let boot_id = sup.boot_id;
            let arena_snap = sup.arena.clone();
            match sup.deploy.prepare_activation(tx_id, now_ns, &arena_snap) {
                Ok(Some(act)) => {
                    let rev_num = act.revision_num;
                    let pkg_hash = act.package_hash.clone();
                    sup.pending_activation = Some(act);
                    drop(sup);
                    self.record_audit(
                        now_ns,
                        boot_id,
                        &actor,
                        &format!("{role:?}"),
                        "ACTIVATE_QUEUED",
                        Some(tx_id),
                        Some(&pkg_hash),
                        rev_num,
                        "QUEUED_FOR_NEXT_BOUNDARY",
                    );
                    return json_resp(
                        202,
                        &json!({"txId": tx_id, "status": "QUEUED_FOR_NEXT_BOUNDARY", "targetRevision": rev_num.to_string()}),
                    );
                }
                Ok(None) => {
                    // Idempotent repeat of activate
                    let tx = sup.deploy.transactions.get(tx_id).unwrap().clone();
                    return json_resp(200, &json!(tx));
                }
                Err(DeployError::UnknownTx(_)) => return json_resp(404, &json!({"error": "UNKNOWN_TX"})),
                Err(e) => return json_resp(409, &json!({"error": "ACTIVATE_REJECTED", "message": e.to_string()})),
            }
        }

        // GET /v1/proposals/{tx}
        if method == "GET" && parts.len() == 3 && parts[1] == "proposals" {
            let tx_id = parts[2];
            let sup = self.supervisor.lock().unwrap();
            if let Some(tx) = sup.deploy.transactions.get(tx_id) {
                return json_resp(
                    200,
                    &json!({
                        "transaction": tx,
                        "activeRevision": sup.deploy.active_revision.to_string(),
                        "lastEvidence": sup.last_evidence,
                    }),
                );
            }
            return json_resp(404, &json!({"error": "UNKNOWN_TX"}));
        }

        // POST /v1/proposals/{tx}/cancel
        if method == "POST"
            && parts.len() == 4
            && parts[1] == "proposals"
            && parts[3] == "cancel"
        {
            if role < Role::Operator {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let tx_id = parts[2];
            let mut sup = self.supervisor.lock().unwrap();
            if let Some(ref pend) = sup.pending_activation {
                if pend.tx_id == tx_id {
                    sup.pending_activation = None;
                }
            }
            let was_armed = sup
                .deploy
                .transactions
                .get(tx_id)
                .is_some_and(|t| t.revision.state == DeploymentState::Armed);
            match sup.deploy.cancel_armed(tx_id) {
                Ok(tx) => {
                    let tx_clone = tx.clone();
                    let outcome = if tx_clone.revision.state == DeploymentState::Rejected {
                        "CANCELLED"
                    } else if matches!(
                        tx_clone.revision.state,
                        DeploymentState::ActiveVolatile | DeploymentState::Stable
                    ) {
                        "ALREADY_ACTIVE"
                    } else {
                        "NOT_ARMED"
                    };
                    let now_ns = sup.clock.now_ns();
                    let boot_id = sup.boot_id;
                    let rev = sup.deploy.active_revision;
                    drop(sup);
                    if was_armed {
                        self.record_audit(
                            now_ns,
                            boot_id,
                            &actor,
                            &format!("{role:?}"),
                            "CANCEL_ARMED",
                            Some(tx_id),
                            Some(&tx_clone.revision.package_hash),
                            rev,
                            outcome,
                        );
                    }
                    return json_resp(200, &json!({"outcome": outcome, "transaction": tx_clone}));
                }
                Err(DeployError::UnknownTx(_)) => return json_resp(404, &json!({"error": "UNKNOWN_TX"})),
                Err(e) => return json_resp(409, &json!({"error": "CANCEL_ERROR", "message": e.to_string()})),
            }
        }

        // POST /v1/devices/{id}/rollbacks
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "rollbacks" {
            if role < Role::Operator {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let rb: RollbackRequestBody = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "message": e.to_string()})),
            };
            let expected_base: u64 = match rb.base_revision.parse() {
                Ok(v) => v,
                Err(_) => {
                    return json_resp(400, &json!({"error": "BAD_REQUEST", "message": "baseRevision must be decimal u64 string"}));
                }
            };
            let mut sup = self.supervisor.lock().unwrap();
            if parts[2] != sup.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            if expected_base != sup.deploy.active_revision {
                return json_resp(
                    409,
                    &json!({"error": "BASE_REVISION_CONFLICT", "expected": sup.deploy.active_revision.to_string()}),
                );
            }
            if !sup.control_armed {
                return json_resp(409, &json!({"error": "CONTROL_DISARMED"}));
            }
            let arena_snap = sup.arena.clone();
            let now_ns = sup.clock.now_ns();
            let boot_id = sup.boot_id;
            match sup.deploy.prepare_rollback(rb.tx_id.clone(), rb.reason, &arena_snap) {
                Ok(act) => {
                    let rev_num = act.revision_num;
                    let pkg_hash = act.package_hash.clone();
                    sup.pending_activation = Some(act);
                    drop(sup);
                    self.record_audit(
                        now_ns,
                        boot_id,
                        &actor,
                        &format!("{role:?}"),
                        "ROLLBACK_QUEUED",
                        Some(&rb.tx_id),
                        Some(&pkg_hash),
                        rev_num,
                        "QUEUED",
                    );
                    return json_resp(
                        202,
                        &json!({"txId": rb.tx_id, "targetRevision": rev_num.to_string(), "status": "QUEUED"}),
                    );
                }
                Err(e) => return json_resp(409, &json!({"error": "ROLLBACK_REJECTED", "message": e.to_string()})),
            }
        }

        // Optional bench manual lease endpoint
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "manual" {
            if role < Role::Operator {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let mb: ManualLeaseBody = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "message": e.to_string()})),
            };
            let mut sup = self.supervisor.lock().unwrap();
            if parts[2] != sup.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let now_ns = sup.clock.now_ns();
            let boot_id = sup.boot_id;
            let rev = sup.deploy.active_revision;
            sup.manual_lease = Some(ManualLease {
                boot_id,
                mask: mb.mask,
                valid_mask: mb.valid_mask,
                expires_logical_ns: now_ns.saturating_add(mb.ttl_ms.min(300_000).saturating_mul(1_000_000)),
                operator_id: actor.clone(),
            });
            drop(sup);
            self.record_audit(
                now_ns,
                boot_id,
                &actor,
                &format!("{role:?}"),
                "MANUAL_LEASE",
                None,
                None,
                rev,
                "APPLIED",
            );
            return json_resp(200, &json!({"ok": true, "expiresInMs": mb.ttl_ms.min(300_000)}));
        }

        // Rearm operator endpoint after boot/latch
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "rearm" {
            if role < Role::Operator {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let mut sup = self.supervisor.lock().unwrap();
            if parts[2] != sup.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            sup.arm_control();
            let armed = sup.control_armed;
            let now_ns = sup.clock.now_ns();
            let boot_id = sup.boot_id;
            let rev = sup.deploy.active_revision;
            drop(sup);
            self.record_audit(
                now_ns,
                boot_id,
                &actor,
                &format!("{role:?}"),
                "REARM",
                None,
                None,
                rev,
                "ARMED",
            );
            return json_resp(200, &json!({"controlArmed": armed}));
        }

        json_resp(404, &json!({"error": "ROUTE_NOT_FOUND"}))
    }
}

fn json_resp(status: u16, val: &Value) -> (u16, &'static str, Vec<u8>) {
    (
        status,
        "application/json; charset=utf-8",
        serde_json::to_vec(val).unwrap_or_else(|_| b"{}".to_vec()),
    )
}

pub fn serve_http_blocking<C: ControlClock + 'static, D: FieldIoDriver + 'static>(
    listener: TcpListener,
    ctx: Arc<ApiContext<C, D>>,
) {
    for stream_res in listener.incoming() {
        if let Ok(stream) = stream_res {
            let ctx_c = Arc::clone(&ctx);
            thread::spawn(move || {
                handle_tcp_client(stream, &ctx_c);
            });
        }
    }
}

fn handle_tcp_client<C: ControlClock, D: FieldIoDriver>(
    mut stream: TcpStream,
    ctx: &ApiContext<C, D>,
) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let Ok(cloned) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(cloned);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 || request_line.len() > 8192 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_target = parts.next().unwrap_or("/");
    let path = raw_target.split('?').next().unwrap_or("/");

    let mut content_len = 0usize;
    let mut auth_header: Option<String> = None;
    let mut header_count = 0usize;
    loop {
        header_count += 1;
        if header_count > 128 {
            return;
        }
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.len() > 8192 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_len = match v.trim().parse::<usize>() {
                    Ok(n) if n <= 1024 * 1024 => n,
                    _ => {
                        let _ = stream.write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                };
            } else if k.eq_ignore_ascii_case("authorization") {
                auth_header = Some(v.trim().to_string());
            }
        }
    }

    let mut body = vec![0u8; content_len];
    if content_len > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }

    if method == "OPTIONS" {
        let hdr = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(hdr.as_bytes());
        return;
    }

    let (status, ctype, resp_body) =
        ctx.handle_http_request(method, path, auth_header.as_deref(), &body);
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        resp_body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&resp_body);
    let _ = stream.flush();
}
