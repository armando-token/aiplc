#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PLC_SSH="${PLC_SSH:-luckfox-usb}"
RUN_ID="${RUN_ID:-night-$(date +%Y%m%d-%H%M%S)}"
PHASE="${PHASE:-all}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --run-id) RUN_ID="$2"; shift 2 ;;
    --phase) PHASE="$2"; shift 2 ;;
    --ssh) PLC_SSH="$2"; shift 2 ;;
    *) echo "Unknown arg: $1"; exit 2 ;;
  esac
done

REMOTE_BASE="/userdata/plc-auto/night/$RUN_ID"
LOCAL_OUT="$REPO_ROOT/docs/benchmarks/artifacts/night/$RUN_ID"
mkdir -p "$LOCAL_OUT"

check_lyra_service() {
  local tag="$1"
  local status
  status="$(ssh "$PLC_SSH" "systemctl is-active lyra-plc.service" 2>/dev/null || true)"
  if [ "$status" != "active" ]; then
    echo "ABORT [$tag]: lyra-plc.service is NOT active (got '$status')" >&2
    exit 1
  fi
  echo "[SAFE CHECK $tag] lyra-plc.service = $status"
}

run_deploy() {
  check_lyra_service "pre-deploy"
  local tmp_pkgs
  tmp_pkgs="$(mktemp -d)"
  trap 'rm -rf "$tmp_pkgs"' RETURN

  # Pre-sign ARMv7 M2 variants for baseRevision=1,3,5 using repo @plc-auto/control-ir crypto
  node - "$REPO_ROOT" "$tmp_pkgs" <<'NODE'
const fs = require("node:fs");
const path = require("node:path");
const cryptoMod = require(path.join(process.argv[2], "packages/control-ir/dist/crypto.js"));
const seedHex = fs.readFileSync(path.join(process.argv[2], "profiles/demo-lyra/keys/test-bench.seed.hex"), "utf8").trim();
const m2Base = JSON.parse(fs.readFileSync(path.join(process.argv[2], "profiles/demo-lyra/packages/M2.armv7-linux-gnueabihf.json"), "utf8"));
for (const rev of [1, 3, 5]) {
  const pkg = { ...m2Base, baseRevision: String(rev) };
  const payload = cryptoMod.modulePackageSigningPayload(pkg);
  pkg.signatureHex = cryptoMod.signBytesEd25519(seedHex, payload);
  fs.writeFileSync(path.join(process.argv[3], `M2.armv7.base${rev}.json`), JSON.stringify(pkg));
}
NODE

  shasum -a 256 \
    "$REPO_ROOT/target/armv7-unknown-linux-gnueabihf/release/plc-supervisor" \
    "$REPO_ROOT/target/armv7-unknown-linux-gnueabihf/release/plc-worker" \
    > "$LOCAL_OUT/armv7_sha256.txt"

  ssh "$PLC_SSH" "mkdir -p $REMOTE_BASE/bin $REMOTE_BASE/profiles $REMOTE_BASE/packages $REMOTE_BASE/data $REMOTE_BASE/metrics"
  scp -q \
    "$REPO_ROOT/target/armv7-unknown-linux-gnueabihf/release/plc-supervisor" \
    "$REPO_ROOT/target/armv7-unknown-linux-gnueabihf/release/plc-worker" \
    "$PLC_SSH:$REMOTE_BASE/bin/"
  scp -q \
    "$REPO_ROOT/profiles/demo-lyra/site-profile.json" \
    "$REPO_ROOT/profiles/demo-lyra/keys/test-bench.pub.hex" \
    "$PLC_SSH:$REMOTE_BASE/profiles/"
  scp -q \
    "$REPO_ROOT/profiles/demo-lyra/packages/M1.armv7-linux-gnueabihf.json" \
    "$tmp_pkgs"/M2.armv7.base*.json \
    "$PLC_SSH:$REMOTE_BASE/packages/"

  ssh "$PLC_SSH" "chmod +x $REMOTE_BASE/bin/* && date > $REMOTE_BASE/host_sync_plc_date.txt"
  check_lyra_service "post-deploy"
  echo "Deployed RUN_ID=$RUN_ID -> $REMOTE_BASE"
}

run_n0_n1() {
  check_lyra_service "pre-n0-n1"
  ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=8 "$PLC_SSH" "python3 - $REMOTE_BASE" <<'PY'
import json, os, shutil, signal, subprocess, sys, time, urllib.error, urllib.request

base = sys.argv[1]
sup_bin = f"{base}/bin/plc-supervisor"
worker_bin = f"{base}/bin/plc-worker"
prof = f"{base}/profiles/site-profile.json"
pub = f"{base}/profiles/test-bench.pub.hex"
m1_pkg = f"{base}/packages/M1.armv7-linux-gnueabihf.json"
port = 18443
url_base = f"http://127.0.0.1:{port}"
results = {}

def http_req(method, path, token=None, body=None, timeout=3):
    hdrs = {}
    if token:
        hdrs["Authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        hdrs["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    req = urllib.request.Request(url_base + path, data=data, headers=hdrs, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, raw

def wait_health(proc):
    for _ in range(100):
        if proc.poll() is not None:
            raise RuntimeError(f"supervisor exited prematurely: rc={proc.returncode}")
        try:
            c, b = http_req("GET", "/health", timeout=1)
            if c == 200:
                return b
        except Exception:
            pass
        time.sleep(0.05)
    raise RuntimeError("health timeout")

def stop_proc(proc):
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

def run_subtest(tag, extra_args, auto_arm=True):
    ddir = f"{base}/data/{tag}"
    mdir = f"{base}/metrics"
    shutil.rmtree(ddir, ignore_errors=True)
    os.makedirs(ddir, exist_ok=True)
    log_path = f"{base}/metrics/{tag}.stdout.log"
    os.makedirs(mdir, exist_ok=True)
    cmd = [
        sup_bin,
        "--profile", prof,
        "--pubkey", pub,
        "--data-dir", ddir,
        "--initial-pkg", m1_pkg,
        "--engine", "wamr-aot",
        "--worker-bin", worker_bin,
        "--strict-sandbox",
        "--silent-night",
        "--no-lcd",
        "--bind", f"127.0.0.1:{port}",
        "--metrics-dir", mdir,
        "--run-id", tag,
    ]
    if auto_arm:
        cmd.append("--auto-arm")
    cmd.extend(extra_args)
    lf = open(log_path, "w")
    p = subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT)
    return p, lf, log_path

# --- N0: Smoke + Silent Profile + /proc/<pid> Sandbox Audit ---
p, lf, log_path = run_subtest("n0-smoke", [])
try:
    h = wait_health(p)
    assert h["ioMode"] == "virtual" and h["serialBackend"] == "none" and h["lcdEnabled"] is False, h
    time.sleep(0.4)
    c, st = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
    assert c == 200, st
    sup_pid = int(st["supervisorPid"])
    wrk_pid = int(st["activeWorkerPid"])
    assert sup_pid != wrk_pid and wrk_pid > 0, st

    # Inspect FDs on supervisor & isolated worker
    sup_fds = [os.readlink(f"/proc/{sup_pid}/fd/{f}") for f in os.listdir(f"/proc/{sup_pid}/fd")]
    wrk_fds = [os.readlink(f"/proc/{wrk_pid}/fd/{f}") for f in os.listdir(f"/proc/{wrk_pid}/fd")]
    forbidden = [x for x in sup_fds + wrk_fds if ("gpiochip" in x or "/dev/ttyS" in x or "kvm_lyra_plc_display" in x)]
    assert not forbidden, f"forbidden physical/lcd FD open: {forbidden}"

    status_txt = open(f"/proc/{wrk_pid}/status").read()
    uid_line = [l for l in status_txt.splitlines() if l.startswith("Uid:")][0]
    seccomp_line = [l for l in status_txt.splitlines() if l.startswith("Seccomp:")][0]
    cg_txt = open(f"/proc/{wrk_pid}/cgroup").read().strip()
    assert "65534" in uid_line, uid_line
    assert seccomp_line.split()[-1] == "2", seccomp_line
    assert "plc-worker-" in cg_txt, cg_txt

    results["N0_smoke"] = {
        "pass": True,
        "health": h,
        "sup_pid": sup_pid,
        "worker_pid": wrk_pid,
        "uid_line": uid_line,
        "seccomp_line": seccomp_line,
        "cgroup": cg_txt,
        "sup_fd_count": len(sup_fds),
        "worker_fd_count": len(wrk_fds),
    }
finally:
    stop_proc(p)
    lf.close()
results["N0_smoke"]["startup_log"] = open(log_path).read().strip()

# --- N1.1: Slow active worker > deadline ---
p, lf, _ = run_subtest("n1_1-slow", ["--worker-fault-arg", "--fault-slow-ms", "--worker-fault-arg", "80", "--worker-fault-arg", "--fault-after-evals", "--worker-fault-arg", "8"])
try:
    wait_health(p)
    st = None
    for _ in range(40):
        _, st = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
        if st.get("fallbackLatched") and st.get("activeWorkerPid") is None:
            break
        time.sleep(0.05)
    assert st["fallbackLatched"] is True and st["activeWorkerPid"] is None and st["appliedMask"] == 0, st
    c_h, _ = http_req("GET", "/health")
    assert c_h == 200
    results["N1_1_slow_deadline"] = {"pass": True, "state": st}
finally:
    stop_proc(p); lf.close()

# --- N1.2a: OOM inside cgroup ---
p, lf, _ = run_subtest("n1_2a-oom", ["--worker-mem-bytes", "8388608", "--worker-fault-arg", "--fault-oom", "--worker-fault-arg", "--fault-after-evals", "--worker-fault-arg", "5"])
try:
    wait_health(p)
    st = None
    for _ in range(40):
        _, st = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
        if st.get("fallbackLatched") and st.get("activeWorkerPid") is None:
            break
        time.sleep(0.05)
    assert st["fallbackLatched"] is True and st["appliedMask"] == 0, st
    results["N1_2a_oom"] = {"pass": True, "state": st}
finally:
    stop_proc(p); lf.close()

# --- N1.2b: Partial EOF inside worker ---
p, lf, _ = run_subtest("n1_2b-partial-eof", ["--worker-fault-arg", "--fault-partial-eof", "--worker-fault-arg", "--fault-after-evals", "--worker-fault-arg", "5"])
try:
    wait_health(p)
    st = None
    for _ in range(40):
        _, st = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
        if st.get("fallbackLatched") and st.get("activeWorkerPid") is None:
            break
        time.sleep(0.05)
    assert st["fallbackLatched"] is True and st["appliedMask"] == 0, st
    results["N1_2b_partial_eof"] = {"pass": True, "state": st}
finally:
    stop_proc(p); lf.close()

# --- N1.3 & N1.5: Async Shadow divergent + wedged ---
p, lf, _ = run_subtest("n1_3_n1_5-shadow", [])
try:
    wait_health(p)
    m2_pkg = json.load(open(f"{base}/packages/M2.armv7.base1.json"))
    c, sh_resp = http_req("POST", "/v1/devices/lyra-bench-01/shadow", "plc-operator-token", {
        "package": m2_pkg,
        "faultArgs": ["--fault-diverge"]
    })
    assert c == 200, (c, sh_resp)
    time.sleep(0.35)
    _, st_div = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
    sh = st_div["shadow"]
    assert sh["scansExecuted"] >= 4 and sh["mismatches"] > 0 and sh["firstCounterexample"], st_div
    assert st_div["fallbackLatched"] is False and st_div["missedReleases"] == 0, st_div
    results["N1_3_shadow_diverge"] = {"pass": True, "shadow": sh, "missedReleases": st_div["missedReleases"]}

    # N1.5 hanging shadow with 80ms shadow timeout (>20ms scan period) -> active loop unblocked, shadow_dropped > 0
    scan0 = st_div["scanId"]
    c, hang_resp = http_req("POST", "/v1/devices/lyra-bench-01/shadow", "plc-operator-token", {
        "package": m2_pkg,
        "faultArgs": ["--fault-hang"],
        "shadowDeadlineMs": 80
    })
    assert c == 200, (c, hang_resp)
    time.sleep(0.35)
    _, st_hang = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
    sh_h = st_hang["shadow"]
    assert (st_hang["scanId"] - scan0) >= 10, st_hang
    assert st_hang["missedReleases"] == 0 and st_hang["fallbackLatched"] is False, st_hang
    assert sh_h["dropped"] > 0 and sh_h["errors"] >= 1, sh_h
    results["N1_5_shadow_hang"] = {"pass": True, "scanDelta": st_hang["scanId"] - scan0, "shadow": sh_h, "missedReleases": st_hang["missedReleases"]}
finally:
    stop_proc(p); lf.close()

# --- N1.4: Syscall escape blocked by ARMv7 Seccomp-BPF ---
p, lf, _ = run_subtest("n1_4-escape", ["--worker-fault-arg", "--fault-escape", "--worker-fault-arg", "--fault-after-evals", "--worker-fault-arg", "5"])
try:
    wait_health(p)
    st = None
    for _ in range(40):
        _, st = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
        if st.get("fallbackLatched"):
            break
        time.sleep(0.05)
    _, audit = http_req("GET", "/v1/devices/lyra-bench-01/audit", "plc-reader-token")
    ev_txt = json.dumps(audit["runtimeEvents"])
    assert "socket_err=1" in ev_txt and "open_err=1" in ev_txt, ev_txt
    results["N1_4_seccomp_escape"] = {"pass": True, "runtimeEvents": audit["runtimeEvents"], "state": st}
finally:
    stop_proc(p); lf.close()

with open(f"{base}/metrics/n0_n1_results.json", "w") as out:
    json.dump(results, out, indent=2)
print(json.dumps({k: v["pass"] for k, v in results.items()}, indent=2))
PY
  check_lyra_service "post-n0-n1"
  scp -q "$PLC_SSH:$REMOTE_BASE/metrics/n0_n1_results.json" "$LOCAL_OUT/"
}

run_n2_baseline() {
  check_lyra_service "pre-n2-baseline"
  ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=60 "$PLC_SSH" "python3 - $REMOTE_BASE" <<'PY'
import json, os, shutil, subprocess, sys, time, urllib.request

base = sys.argv[1]
ddir = f"{base}/data/gate2-baseline-10m"
mdir = f"{base}/metrics"
shutil.rmtree(ddir, ignore_errors=True)
shutil.rmtree(f"{mdir}/gate2-load-3m", ignore_errors=True)
os.makedirs(ddir, exist_ok=True)
os.makedirs(mdir, exist_ok=True)
port = 18443
url_base = f"http://127.0.0.1:{port}"

cmd = [
    f"{base}/bin/plc-supervisor",
    "--profile", f"{base}/profiles/site-profile.json",
    "--pubkey", f"{base}/profiles/test-bench.pub.hex",
    "--data-dir", ddir,
    "--initial-pkg", f"{base}/packages/M1.armv7-linux-gnueabihf.json",
    "--engine", "wamr-aot",
    "--worker-bin", f"{base}/bin/plc-worker",
    "--strict-sandbox",
    "--silent-night",
    "--no-lcd",
    "--rt-fifo",
    "--bind", f"127.0.0.1:{port}",
    "--metrics-dir", mdir,
    "--run-id", "gate2-baseline-10m",
    "--stop-after-s", "600",
]
lf = open(f"{mdir}/gate2-baseline-10m.stdout.log", "w")
p = subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT)
try:
    for _ in range(100):
        try:
            with urllib.request.urlopen(url_base + "/health", timeout=1) as r:
                if r.status == 200:
                    break
        except Exception:
            time.sleep(0.05)

    # Rearm cleanly over loopback API after verifying disarmed boot
    r_st0 = urllib.request.Request(url_base + "/v1/devices/lyra-bench-01/state", headers={"Authorization": "Bearer plc-reader-token"})
    st0 = json.loads(urllib.request.urlopen(r_st0, timeout=2).read().decode())
    assert st0["armed"] is False, st0

    r_rearm = urllib.request.Request(url_base + "/v1/devices/lyra-bench-01/rearm", data=b"{}", headers={"Authorization": "Bearer plc-operator-token", "Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(r_rearm, timeout=2) as resp:
        assert resp.status == 200

    rc = p.wait(timeout=660)
    assert rc == 0, f"baseline exited with rc={rc}"
finally:
    if p.poll() is None:
        p.kill()
        p.wait()
    lf.close()

summary = json.load(open(f"{mdir}/gate2-baseline-10m/summary.json"))
print(json.dumps({
    "scansTotal": summary["scansTotal"],
    "deadlineMisses": summary["deadlineMisses"],
    "missedReleases": summary["missedReleases"],
    "wakeup_p99_ns": summary["wakeupJitter"]["p99Ns"],
    "wakeup_max_ns": summary["wakeupJitter"]["maxNs"],
    "cycle_p99_ns": summary["cycleDur"]["p99Ns"],
    "cycle_max_ns": summary["cycleDur"]["maxNs"],
}, indent=2))
PY
  check_lyra_service "post-n2-baseline"
  mkdir -p "$LOCAL_OUT/gate2-baseline-10m"
  scp -q \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-baseline-10m/manifest.json" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-baseline-10m/summary.json" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-baseline-10m/histograms.json" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-baseline-10m/samples.csv" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-baseline-10m/anomalies.csv" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-baseline-10m/events.jsonl" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-baseline-10m.stdout.log" \
    "$LOCAL_OUT/gate2-baseline-10m/"
}

run_n2_load() {
  check_lyra_service "pre-n2-load"
  ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=30 "$PLC_SSH" "python3 - $REMOTE_BASE" <<'PY'
import json, os, shutil, subprocess, sys, threading, time, urllib.error, urllib.request

base = sys.argv[1]
ddir = f"{base}/data/gate2-load-3m"
mdir = f"{base}/metrics"
shutil.rmtree(ddir, ignore_errors=True)
shutil.rmtree(f"{mdir}/gate2-load-3m", ignore_errors=True)
os.makedirs(ddir, exist_ok=True)
port = 18443
url_base = f"http://127.0.0.1:{port}"

cmd = [
    f"{base}/bin/plc-supervisor",
    "--profile", f"{base}/profiles/site-profile.json",
    "--pubkey", f"{base}/profiles/test-bench.pub.hex",
    "--data-dir", ddir,
    "--initial-pkg", f"{base}/packages/M1.armv7-linux-gnueabihf.json",
    "--engine", "wamr-aot",
    "--worker-bin", f"{base}/bin/plc-worker",
    "--strict-sandbox",
    "--silent-night",
    "--no-lcd",
    "--rt-fifo",
    "--bind", f"127.0.0.1:{port}",
    "--metrics-dir", mdir,
    "--run-id", "gate2-load-3m",
    "--stop-after-s", "185",
]
lf = open(f"{mdir}/gate2-load-3m.stdout.log", "w")
p = subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT)

def http_req(method, path, token, body=None, timeout=4):
    hdrs = {"Authorization": f"Bearer {token}"}
    data = None
    if body is not None:
        hdrs["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    r = urllib.request.Request(url_base + path, data=data, headers=hdrs, method=method)
    with urllib.request.urlopen(r, timeout=timeout) as resp:
        return resp.status, json.loads(resp.read().decode())

try:
    for _ in range(100):
        try:
            with urllib.request.urlopen(url_base + "/health", timeout=1) as r:
                if r.status == 200:
                    break
        except Exception:
            time.sleep(0.05)

    c, _ = http_req("POST", "/v1/devices/lyra-bench-01/rearm", "plc-operator-token", {})
    assert c == 200

    # Concurrent API + virtual-inputs flood threads
    stop_load = threading.Event()
    thermal_phase = threading.Event()
    load_stats = {"ok": 0, "busy429": 0, "err": 0}
    lock = threading.Lock()
    def spammer(tid):
        quiet_temps = [250, 260, 280]
        hot_temps = [250, 425, 455]
        i = 0
        while not stop_load.is_set():
            i += 1
            try:
                if tid == 3 and i % 10 == 0:
                    palette = hot_temps if thermal_phase.is_set() else quiet_temps
                    c_r, _ = http_req("POST", "/v1/devices/lyra-bench-01/virtual-inputs", "plc-operator-token", {"tempDecic": palette[i % 3]})
                elif i % 3 == 0:
                    c_r, _ = http_req("GET", "/v1/devices/lyra-bench-01/scans", "plc-reader-token")
                else:
                    c_r, _ = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
                with lock:
                    if c_r == 200: load_stats["ok"] += 1
            except urllib.error.HTTPError as e:
                with lock:
                    if e.code == 429: load_stats["busy429"] += 1
                    else: load_stats["err"] += 1
            except Exception:
                with lock:
                    load_stats["err"] += 1
            time.sleep(0.02)

    threads = [threading.Thread(target=spammer, args=(t,), daemon=True) for t in range(4)]
    for th in threads: th.start()

    # Install async shadow & run 3 swap/rollback cycles spread over first ~95s
    m2_b1 = json.load(open(f"{base}/packages/M2.armv7.base1.json"))
    m2_b3 = json.load(open(f"{base}/packages/M2.armv7.base3.json"))
    m2_b5 = json.load(open(f"{base}/packages/M2.armv7.base5.json"))
    cycles = [(1, 2, 3, m2_b1), (3, 4, 5, m2_b3), (5, 6, 7, m2_b5)]
    for idx, (base_r, target_r, rb_r, pkg) in enumerate(cycles):
        time.sleep(12)
        tx = f"load-tx-{target_r}"
        c1, prop = http_req("POST", "/v1/devices/lyra-bench-01/proposals", "plc-author-token", {"txId": tx, "package": pkg})
        assert c1 == 201, prop
        h = prop["revision"]["packageHash"]
        c2, sh_r = http_req("POST", f"/v1/proposals/{tx}/shadow", "plc-operator-token", {"waitMs": 100})
        assert c2 == 200, (c2, sh_r)
        assert sh_r["revision"]["state"] == "SHADOW", sh_r
        c3, au_r = http_req("POST", f"/v1/proposals/{tx}/authorizations", "plc-operator-token", {"operatorId": "night-op", "expectedPackageHash": h, "ttlMs": 60000})
        assert c3 == 200, (c3, au_r)
        c4, ac_r = http_req("POST", f"/v1/proposals/{tx}/activate", "plc-operator-token", {})
        assert c4 == 202, (c4, ac_r)
        for _ in range(40):
            _, st = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
            if st["activeRevision"] == target_r and st["activePersisted"]:
                break
            time.sleep(0.05)
        assert st["activeRevision"] == target_r and st["activePersisted"], st

        time.sleep(12)
        c_rb, _ = http_req("POST", "/v1/devices/lyra-bench-01/rollback", "plc-operator-token", {"reason": f"cycle_{idx}_rb"})
        assert c_rb == 202
        for _ in range(40):
            _, st2 = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
            if st2["activeRevision"] == rb_r and st2["activePersisted"]:
                break
            time.sleep(0.05)
        assert st2["activeRevision"] == rb_r and st2["activePersisted"], st2

    # Enable hot thermal excursions + live AsyncShadowRunner for remaining ~110s of load test
    thermal_phase.set()
    http_req("POST", "/v1/devices/lyra-bench-01/shadow", "plc-operator-token", {"package": m2_b5})

    rc = p.wait(timeout=220)
    stop_load.set()
    for th in threads: th.join(timeout=2)
    assert rc == 0, f"load run exited with rc={rc}"
finally:
    if p.poll() is None:
        p.kill(); p.wait()
    lf.close()

with open(f"{mdir}/gate2-load-3m/api_load_stats.json", "w") as out:
    json.dump(load_stats, out, indent=2)
summary = json.load(open(f"{mdir}/gate2-load-3m/summary.json"))
print(json.dumps({"load_stats": load_stats, "summary": summary}, indent=2))
PY
  check_lyra_service "post-n2-load"
  mkdir -p "$LOCAL_OUT/gate2-load-3m"
  scp -q \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-load-3m/manifest.json" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-load-3m/summary.json" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-load-3m/histograms.json" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-load-3m/samples.csv" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-load-3m/anomalies.csv" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-load-3m/events.jsonl" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-load-3m/api_load_stats.json" \
    "$PLC_SSH:$REMOTE_BASE/metrics/gate2-load-3m.stdout.log" \
    "$LOCAL_OUT/gate2-load-3m/"
}

run_n2_recovery() {
  check_lyra_service "pre-n2-recovery"
  ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=8 "$PLC_SSH" "python3 - $REMOTE_BASE" <<'PY'
import json, os, shutil, signal, subprocess, sys, time, urllib.request

base = sys.argv[1]
sup_bin = f"{base}/bin/plc-supervisor"
worker_bin = f"{base}/bin/plc-worker"
prof = f"{base}/profiles/site-profile.json"
pub = f"{base}/profiles/test-bench.pub.hex"
m1_pkg = f"{base}/packages/M1.armv7-linux-gnueabihf.json"
m2_b1 = json.load(open(f"{base}/packages/M2.armv7.base1.json"))
port = 18443
url_base = f"http://127.0.0.1:{port}"
res = {}

def http_req(method, path, token=None, body=None):
    hdrs = {}
    if token: hdrs["Authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        hdrs["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    r = urllib.request.Request(url_base + path, data=data, headers=hdrs, method=method)
    with urllib.request.urlopen(r, timeout=3) as resp:
        return resp.status, json.loads(resp.read().decode())

def wait_health(proc):
    for _ in range(100):
        if proc.poll() is not None:
            raise RuntimeError(f"exited early: {proc.returncode}")
        try:
            c, _ = http_req("GET", "/health")
            if c == 200: return
        except Exception:
            pass
        time.sleep(0.05)
    raise RuntimeError("health timeout")

# Case A: Crash AFTER_VOLATILE_BEFORE_PERSISTED (_exit(99))
ddir_a = f"{base}/data/rec-case-a"
shutil.rmtree(ddir_a, ignore_errors=True)
os.makedirs(ddir_a, exist_ok=True)
# Pre-seed M1 rev 1 as Stable cleanly before injecting crash point for M2 promotion
p0 = subprocess.Popen([
    sup_bin, "--profile", prof, "--pubkey", pub, "--data-dir", ddir_a, "--initial-pkg", m1_pkg,
    "--engine", "wamr-aot", "--worker-bin", worker_bin, "--strict-sandbox", "--silent-night", "--no-lcd", "--bind", f"127.0.0.1:{port}"
], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    wait_health(p0)
finally:
    p0.terminate(); p0.wait()

env_a = os.environ.copy()
env_a["PLC_CRASH_POINT"] = "AFTER_VOLATILE_BEFORE_PERSISTED"
p1 = subprocess.Popen([
    sup_bin, "--profile", prof, "--pubkey", pub, "--data-dir", ddir_a,
    "--engine", "wamr-aot", "--worker-bin", worker_bin, "--strict-sandbox", "--silent-night", "--no-lcd", "--bind", f"127.0.0.1:{port}"
], env=env_a, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    wait_health(p1)
    http_req("POST", "/v1/devices/lyra-bench-01/rearm", "plc-operator-token", {})
    _, prop = http_req("POST", "/v1/devices/lyra-bench-01/proposals", "plc-author-token", {"txId": "rec-a-m2", "package": m2_b1})
    h = prop["revision"]["packageHash"]
    http_req("POST", "/v1/proposals/rec-a-m2/authorizations", "plc-operator-token", {"operatorId": "op", "expectedPackageHash": h, "ttlMs": 60000})
    http_req("POST", "/v1/proposals/rec-a-m2/activate", "plc-operator-token", {})
    rc = p1.wait(timeout=10)
    assert rc == 99, f"expected rc=99, got {rc}"
finally:
    if p1.poll() is None: p1.kill(); p1.wait()

p2 = subprocess.Popen([
    sup_bin, "--profile", prof, "--pubkey", pub, "--data-dir", ddir_a,
    "--engine", "wamr-aot", "--worker-bin", worker_bin, "--strict-sandbox", "--silent-night", "--no-lcd", "--bind", f"127.0.0.1:{port}"
], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    wait_health(p2)
    _, st_a = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
    assert st_a["activeRevision"] == 1 and st_a["armed"] is False and st_a["needsOperatorReview"] is True, st_a
    res["caseA_crash_before_persisted"] = {"pass": True, "state": st_a}
finally:
    p2.terminate(); p2.wait()

# Case B: Real SIGKILL after durable Persisted/Stable commit of rev 2
ddir_b = f"{base}/data/rec-case-b"
shutil.rmtree(ddir_b, ignore_errors=True)
os.makedirs(ddir_b, exist_ok=True)
p3 = subprocess.Popen([
    sup_bin, "--profile", prof, "--pubkey", pub, "--data-dir", ddir_b, "--initial-pkg", m1_pkg,
    "--engine", "wamr-aot", "--worker-bin", worker_bin, "--strict-sandbox", "--silent-night", "--no-lcd", "--bind", f"127.0.0.1:{port}"
], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    wait_health(p3)
    http_req("POST", "/v1/devices/lyra-bench-01/rearm", "plc-operator-token", {})
    _, prop = http_req("POST", "/v1/devices/lyra-bench-01/proposals", "plc-author-token", {"txId": "rec-b-m2", "package": m2_b1})
    h = prop["revision"]["packageHash"]
    http_req("POST", "/v1/proposals/rec-b-m2/authorizations", "plc-operator-token", {"operatorId": "op", "expectedPackageHash": h, "ttlMs": 60000})
    http_req("POST", "/v1/proposals/rec-b-m2/activate", "plc-operator-token", {})
    for _ in range(40):
        _, st_mid = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
        if st_mid["activeRevision"] == 2 and st_mid["activePersisted"]:
            break
        time.sleep(0.05)
    assert st_mid["activeRevision"] == 2 and st_mid["activePersisted"], st_mid
    # Kill child worker PID cleanly too or let kernel SIGKILL supervisor (reaper orphans check)
    os.kill(p3.pid, signal.SIGKILL)
    p3.wait()
finally:
    if p3.poll() is None: p3.kill(); p3.wait()

p4 = subprocess.Popen([
    sup_bin, "--profile", prof, "--pubkey", pub, "--data-dir", ddir_b,
    "--engine", "wamr-aot", "--worker-bin", worker_bin, "--strict-sandbox", "--silent-night", "--no-lcd", "--bind", f"127.0.0.1:{port}"
], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    wait_health(p4)
    _, st_b = http_req("GET", "/v1/devices/lyra-bench-01/state", "plc-reader-token")
    assert st_b["activeRevision"] == 2 and st_b["armed"] is False and st_b["needsOperatorReview"] is False and st_b["activePersisted"] is True, st_b
    res["caseB_sigkill_after_stable"] = {"pass": True, "state": st_b}
finally:
    p4.terminate(); p4.wait()

with open(f"{base}/metrics/n2_recovery_results.json", "w") as out:
    json.dump(res, out, indent=2)
print(json.dumps(res, indent=2))
PY
  check_lyra_service "post-n2-recovery"
  scp -q "$PLC_SSH:$REMOTE_BASE/metrics/n2_recovery_results.json" "$LOCAL_OUT/"
}

run_n2_24h() {
  check_lyra_service "pre-n2-24h"
  # Evaluate Gate 1 & Gate 2 short thresholds before launching detached 24h run
  local ok_to_launch
  ok_to_launch="$(python3 - "$LOCAL_OUT" <<'PY'
import json, sys
out = sys.argv[1]
n1 = json.load(open(f"{out}/n0_n1_results.json"))
rec = json.load(open(f"{out}/n2_recovery_results.json"))
s_base = json.load(open(f"{out}/gate2-baseline-10m/summary.json"))
s_load = json.load(open(f"{out}/gate2-load-3m/summary.json"))
reasons = []
for k, v in n1.items():
    if not v.get("pass"): reasons.append(f"{k} failed")
for k, v in rec.items():
    if not v.get("pass"): reasons.append(f"{k} failed")
for label, s in [("baseline-10m", s_base), ("load-3m", s_load)]:
    if s["deadlineMisses"] > 0: reasons.append(f"{label} deadlineMisses={s['deadlineMisses']}")
    if s["missedReleases"] > 0: reasons.append(f"{label} missedReleases={s['missedReleases']}")
    if s["telemetryDropped"] > 0: reasons.append(f"{label} telemetryDropped={s['telemetryDropped']}")
    if s["wakeupJitter"]["p99Ns"] >= 500_000: reasons.append(f"{label} wakeup_p99={s['wakeupJitter']['p99Ns']}")
    if s["wakeupJitter"]["maxNs"] >= 2_000_000: reasons.append(f"{label} wakeup_max={s['wakeupJitter']['maxNs']}")
    if s["cycleDur"]["p99Ns"] >= 2_000_000: reasons.append(f"{label} cycle_p99={s['cycleDur']['p99Ns']}")
if reasons:
    print("NO|" + "; ".join(reasons))
else:
    print("YES|all short gates passed jitter and safety criteria")
PY
)"
  echo "24h Gate Check: $ok_to_launch"
  local decision="${ok_to_launch%%|*}"
  local detail="${ok_to_launch#*|}"
  if [ "$decision" != "YES" ]; then
    python3 - "$LOCAL_OUT/run_24h_status.json" "$detail" <<'PY'
import json, sys
json.dump({"status": "PENDIENTE", "reason": sys.argv[2]}, open(sys.argv[1], "w"), indent=2)
PY
    return 0
  fi

  local dir_24h="${REMOTE_BASE}-24h"
  ssh "$PLC_SSH" "bash -s" <<EOF
set -euo pipefail
mkdir -p "$dir_24h/data" "$dir_24h/metrics"
setsid nohup "$REMOTE_BASE/bin/plc-supervisor" \
  --profile "$REMOTE_BASE/profiles/site-profile.json" \
  --pubkey "$REMOTE_BASE/profiles/test-bench.pub.hex" \
  --data-dir "$dir_24h/data" \
  --initial-pkg "$REMOTE_BASE/packages/M1.armv7-linux-gnueabihf.json" \
  --engine wamr-aot \
  --worker-bin "$REMOTE_BASE/bin/plc-worker" \
  --strict-sandbox \
  --silent-night \
  --no-lcd \
  --rt-fifo \
  --bind 127.0.0.1:18445 \
  --metrics-dir "$dir_24h/metrics" \
  --run-id gate2-24h \
  --max-raw-scan-rows 100000 \
  --stop-after-s 86400 \
  > "$dir_24h/supervisor_24h.log" 2>&1 < /dev/null &
SUP_PID=\$!
echo "\$SUP_PID" > "$dir_24h/supervisor_24h.pid"
sleep 1
PGID=\$(ps -o pgid= -p "\$SUP_PID" | tr -d ' ')
echo "\$PGID" > "$dir_24h/supervisor_24h.pgid"
python3 - <<'REARM'
import time, urllib.request
for _ in range(80):
    try:
        with urllib.request.urlopen("http://127.0.0.1:18445/health", timeout=1) as r:
            if r.status == 200: break
    except Exception:
        time.sleep(0.05)
req = urllib.request.Request(
    "http://127.0.0.1:18445/v1/devices/lyra-bench-01/rearm",
    data=b"{}",
    headers={"Authorization": "Bearer plc-operator-token", "Content-Type": "application/json"},
    method="POST"
)
with urllib.request.urlopen(req, timeout=2) as r:
    assert r.status == 200
REARM
cat > "$dir_24h/stop_24h.sh" <<'STOP'
#!/usr/bin/env bash
set -euo pipefail
DIR="\$(cd "\$(dirname "\$0")" && pwd)"
PGID="\$(cat "\$DIR/supervisor_24h.pgid")"
kill -TERM -"\$PGID" || true
sleep 2
kill -KILL -"\$PGID" 2>/dev/null || true
echo "Stopped 24h PGID=\$PGID (lyra-plc.service untouched)"
STOP
chmod +x "$dir_24h/stop_24h.sh"
EOF

  echo "Waiting 30s to confirm 24h detached health..."
  sleep 30
  local host_iso
  host_iso="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  local host_local
  host_local="$(date '+%Y-%m-%d %H:%M:%S %Z')"
  ssh "$PLC_SSH" "python3 - $dir_24h \"$host_iso\" \"$host_local\"" <<'PY' > "$LOCAL_OUT/run_24h_status.json"
import json, os, subprocess, sys, urllib.request
d = sys.argv[1]
host_utc = sys.argv[2]
host_local = sys.argv[3]
pid = int(open(f"{d}/supervisor_24h.pid").read().strip())
pgid = int(open(f"{d}/supervisor_24h.pgid").read().strip())
plc_date = subprocess.check_output(["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"]).decode().strip()
lyra_svc = subprocess.check_output(["systemctl", "is-active", "lyra-plc.service"]).decode().strip()
with urllib.request.urlopen("http://127.0.0.1:18445/health", timeout=2) as r:
    h = json.loads(r.read().decode())
req = urllib.request.Request("http://127.0.0.1:18445/v1/devices/lyra-bench-01/state", headers={"Authorization": "Bearer plc-reader-token"})
st = json.loads(urllib.request.urlopen(req, timeout=2).read().decode())
assert st["scanId"] >= 1000 and st["missedReleases"] == 0 and lyra_svc == "active", (st, lyra_svc)
print(json.dumps({
    "status": "EN CURSO",
    "dir": d,
    "pid": pid,
    "pgid": pgid,
    "workerPid": st["activeWorkerPid"],
    "hostStartLocal": host_local,
    "hostStartUtc": host_utc,
    "plcStartUtc": plc_date,
    "scanIdAt30s": st["scanId"],
    "missedReleasesAt30s": st["missedReleases"],
    "lyraService": lyra_svc,
    "stopScript": f"{d}/stop_24h.sh",
    "log": f"{d}/supervisor_24h.log",
    "metricsDir": f"{d}/metrics/gate2-24h"
}, indent=2))
PY
  check_lyra_service "post-n2-24h"
  cat "$LOCAL_OUT/run_24h_status.json"
}

case "$PHASE" in
  deploy) run_deploy ;;
  n0_n1) run_n0_n1 ;;
  n2_baseline) run_n2_baseline ;;
  n2_load) run_n2_load ;;
  n2_recovery) run_n2_recovery ;;
  n2_24h) run_n2_24h ;;
  all)
    run_deploy
    run_n0_n1
    run_n2_baseline
    run_n2_load
    run_n2_recovery
    run_n2_24h
    ;;
  *) echo "Invalid PHASE=$PHASE"; exit 2 ;;
esac
