#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PLC_SSH="${PLC_SSH:-luckfox-usb}"
PLC_DIR="/userdata/plc-auto"
LOCAL_OUT="$REPO_ROOT/docs/benchmarks/artifacts"

echo "=== 1. Deploying ARMv7 binaries, profiles, packages, and offline console to $PLC_SSH:$PLC_DIR ==="
ssh "$PLC_SSH" "mkdir -p $PLC_DIR/bin $PLC_DIR/profiles $PLC_DIR/packages $PLC_DIR/data $PLC_DIR/benchmarks $PLC_DIR/console"
scp "$REPO_ROOT/target/armv7-unknown-linux-gnueabihf/release/plc-supervisor" \
    "$REPO_ROOT/target/armv7-unknown-linux-gnueabihf/release/plc-worker" \
    "$REPO_ROOT/target/armv7-unknown-linux-gnueabihf/release/plc-bench" \
    "$PLC_SSH:$PLC_DIR/bin/"
scp "$REPO_ROOT/profiles/demo-lyra/site-profile.json" \
    "$REPO_ROOT/profiles/demo-lyra/bench-w3-profile.json" \
    "$REPO_ROOT/profiles/demo-lyra/keys/test-bench.pub.hex" \
    "$REPO_ROOT/profiles/demo-lyra/keys/test-bench.seed.hex" \
    "$PLC_SSH:$PLC_DIR/profiles/"
scp "$REPO_ROOT/profiles/demo-lyra/packages/"*.armv7-linux-gnueabihf.json \
    "$REPO_ROOT/profiles/demo-lyra/packages/"*.quickjs.json \
    "$PLC_SSH:$PLC_DIR/packages/"
scp "$REPO_ROOT/packages/console/dist-web/"* "$PLC_SSH:$PLC_DIR/console/"

cleanup_plc() {
  echo "=== Cleanup: ensuring PLC outputs safe OFF & restoring lyra-plc.service ==="
  ssh "$PLC_SSH" "pkill -TERM plc-supervisor || true; sleep 0.5; pkill -KILL plc-supervisor || true; systemctl start lyra-plc.service || true" || true
}
trap cleanup_plc EXIT

echo "=== 2. Stopping lyra-plc.service temporarily for exclusive GPIO & RT benchmarks ==="
ssh "$PLC_SSH" "systemctl stop lyra-plc.service && chmod +x $PLC_DIR/bin/*"

echo "=== 3. Running Scheduler Jitter (idle & stress) on real Luckfox PLC ==="
ssh "$PLC_SSH" "
  rm -rf $PLC_DIR/benchmarks/* &&
  $PLC_DIR/bin/plc-bench jitter --out $PLC_DIR/benchmarks/jitter-idle --scans 500 --period-ms 20 --load none &&
  $PLC_DIR/bin/plc-bench jitter --out $PLC_DIR/benchmarks/jitter-stress --scans 500 --period-ms 20 --load stress
"

echo "=== 4. Running A0/A1-interp/A1-aot/A2 benchmarks (W0-W5, active / shadow / shadow-load) on PLC ==="
ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=8 "$PLC_SSH" "set -e
  $PLC_DIR/bin/plc-bench run --engine a0-empty --workload W0 --mode active --scans 400 --profile $PLC_DIR/profiles/site-profile.json --pubkey $PLC_DIR/profiles/test-bench.pub.hex --pkg-dir $PLC_DIR/packages --worker-bin $PLC_DIR/bin/plc-worker --out $PLC_DIR/benchmarks/a0-empty-W0-active
  for wl in W0 W1 W2 W3 W4 W5; do
    prof=$PLC_DIR/profiles/site-profile.json
    if [ \"\$wl\" = \"W3\" ]; then prof=$PLC_DIR/profiles/bench-w3-profile.json; fi
    for eng in a0-ir a1-interp a1-aot a2-quickjs; do
      $PLC_DIR/bin/plc-bench run --engine \$eng --workload \$wl --mode active --scans 350 --profile \$prof --pubkey $PLC_DIR/profiles/test-bench.pub.hex --pkg-dir $PLC_DIR/packages --worker-bin $PLC_DIR/bin/plc-worker --out $PLC_DIR/benchmarks/\${eng}-\${wl}-active
    done
  done
  for eng in a1-aot a2-quickjs; do
    $PLC_DIR/bin/plc-bench run --engine \$eng --workload W1 --mode shadow --scans 350 --profile $PLC_DIR/profiles/site-profile.json --pubkey $PLC_DIR/profiles/test-bench.pub.hex --pkg-dir $PLC_DIR/packages --worker-bin $PLC_DIR/bin/plc-worker --out $PLC_DIR/benchmarks/\${eng}-W1-shadow
  done
  for eng in a0-ir a1-interp a1-aot a2-quickjs; do
    $PLC_DIR/bin/plc-bench run --engine \$eng --workload W1 --mode shadow-load --scans 350 --profile $PLC_DIR/profiles/site-profile.json --pubkey $PLC_DIR/profiles/test-bench.pub.hex --pkg-dir $PLC_DIR/packages --worker-bin $PLC_DIR/bin/plc-worker --out $PLC_DIR/benchmarks/\${eng}-W1-shadow-load
  done
"

echo "=== 5. Running Full Hardware & Sandbox HIL Suite (H01, H02, F01-F06, D03 SIGKILL, S03) ==="
ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=8 "$PLC_SSH" "set -e
  $PLC_DIR/bin/plc-bench hil --hardware-gpio --profile $PLC_DIR/profiles/site-profile.json --pubkey $PLC_DIR/profiles/test-bench.pub.hex --pkg-dir $PLC_DIR/packages --worker-bin $PLC_DIR/bin/plc-worker --supervisor-bin $PLC_DIR/bin/plc-supervisor --data-dir $PLC_DIR/data --out $PLC_DIR/benchmarks/hil
"

echo "=== 6. Running >= 1000 Swaps/Rollbacks Accelerated Soak on PLC eMMC ==="
ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=8 "$PLC_SSH" "set -e
  $PLC_DIR/bin/plc-bench soak --swaps 1000 --profile $PLC_DIR/profiles/site-profile.json --pubkey $PLC_DIR/profiles/test-bench.pub.hex --pkg-dir $PLC_DIR/packages --data-dir $PLC_DIR/data/soak --out $PLC_DIR/benchmarks/soak-1000
"

echo "=== 7. Live REST API + Offline Console + Hot-Swap M1->M2->Rollback Demo on real GPIO ==="
ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=8 "$PLC_SSH" "python3 - <<'PY'
import hashlib, json, subprocess, time, urllib.request, os

data_dir = '$PLC_DIR/data/live-demo'
os.system(f'rm -rf {data_dir}')
p = subprocess.Popen([
    '$PLC_DIR/bin/plc-supervisor',
    '--profile', '$PLC_DIR/profiles/site-profile.json',
    '--pubkey', '$PLC_DIR/profiles/test-bench.pub.hex',
    '--data-dir', data_dir,
    '--initial-pkg', '$PLC_DIR/packages/M1.armv7-linux-gnueabihf.json',
    '--console-dir', '$PLC_DIR/console',
    '--engine', 'wamr-aot',
    '--bind', '127.0.0.1:8443',
    '--hardware-gpio',
    '--auto-arm',
])
try:
    for _ in range(40):
        try:
            with urllib.request.urlopen('http://127.0.0.1:8443/health', timeout=1) as r:
                if r.status == 200:
                    break
        except Exception:
            time.sleep(0.05)

    def req(method, path, token, body=None):
        hdrs = {'Authorization': f'Bearer {token}', 'Content-Type': 'application/json'}
        data = json.dumps(body).encode() if body is not None else None
        r = urllib.request.Request('http://127.0.0.1:8443' + path, data=data, headers=hdrs, method=method)
        with urllib.request.urlopen(r, timeout=3) as resp:
            return resp.status, json.loads(resp.read().decode())

    with urllib.request.urlopen('http://127.0.0.1:8443/console/', timeout=2) as cr:
        html = cr.read().decode()
        assert 'SELECTED_IN_SCAN' in html and 'VIRTUAL' in html

    st0_code, st0 = req('GET', '/v1/devices/lyra-bench-01/state', 'plc-reader-token')
    assert st0['activeRevision'] == '1', st0

    with open('$PLC_DIR/packages/M2.armv7-linux-gnueabihf.json') as f:
        m2 = json.load(f)
    # Use M2 already signed with baseRevision='1' from toolchain
    st_p, prop = req('POST', '/v1/devices/lyra-bench-01/proposals', 'plc-author-token', {'txId': 'demo-m2', 'package': m2})
    h2 = prop['revision']['packageHash']
    st_a, _ = req('POST', '/v1/proposals/demo-m2/authorizations', 'plc-operator-token', {'operatorId': 'op-armando', 'expectedPackageHash': h2, 'ttlMs': 60000})
    st_act, _ = req('POST', '/v1/proposals/demo-m2/activate', 'plc-operator-token', {})
    time.sleep(0.08)

    _, st1 = req('GET', '/v1/devices/lyra-bench-01/state', 'plc-reader-token')
    assert st1['activeRevision'] == '2', st1

    _, rb = req('POST', '/v1/devices/lyra-bench-01/rollbacks', 'plc-operator-token', {'txId': 'demo-rb', 'baseRevision': '2', 'reason': 'demo rollback to M1'})
    time.sleep(0.08)
    _, st2 = req('GET', '/v1/devices/lyra-bench-01/state', 'plc-reader-token')
    assert st2['activeRevision'] == '3', st2

    res = {'consoleServed': True, 'rev0': st0['activeRevision'], 'revAfterM2': st1['activeRevision'], 'revAfterRollback': st2['activeRevision'], 'evidence': st2['lastEvidence']}
    with open('$PLC_DIR/benchmarks/live_demo_result.json', 'w') as out:
        json.dump(res, out, indent=2)
    print(json.dumps(res, indent=2))
finally:
    p.terminate()
    p.wait(timeout=5)
PY"

echo "=== 8. Pulling PLC benchmark & HIL artifacts into $LOCAL_OUT/plc-lyra ==="
rm -rf "$LOCAL_OUT/plc-lyra"
mkdir -p "$LOCAL_OUT/plc-lyra"
scp -r "$PLC_SSH:$PLC_DIR/benchmarks/"* "$LOCAL_OUT/plc-lyra/"
echo "=== DONE PLC HIL & BENCHMARKS ==="
