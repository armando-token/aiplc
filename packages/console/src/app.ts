export function renderConsoleHtml(): string {
  return `<!doctype html>
<html lang="es">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>Luckfox Lyra PLC — Consola Offline</title>
  <link rel="stylesheet" href="/console/styles.css">
</head>
<body>
  <header class="topbar">
    <div>
      <h1>PLC Automatico · Consola Industrial Offline</h1>
      <div id="conn-status" class="muted">Verificando estado...</div>
    </div>
    <div class="auth-bar">
      <input id="device-id" value="lyra-bench-01" aria-label="Device ID">
      <select id="role-token" aria-label="Token de rol">
        <option value="plc-operator-token">Operador</option>
        <option value="plc-engineer-token">Ingeniero</option>
        <option value="plc-author-token">Autor</option>
        <option value="plc-reader-token">Lector</option>
      </select>
      <button id="btn-refresh" type="button">Actualizar</button>
    </div>
  </header>

  <main class="grid">
    <section class="card">
      <h2>Estado en Tiempo Real &amp; Evidencia (D04)</h2>
      <div id="pending-banner" class="banner hidden"></div>
      <div id="witness-banner" class="banner warn hidden"></div>
      <div class="kv-grid">
        <div><span>Revision Activa</span><strong id="val-rev">-</strong></div>
        <div><span>Armado</span><strong id="val-armed">-</strong></div>
        <div><span>Scan / Boot</span><strong id="val-scan">-</strong></div>
        <div><span>Fallback Latch</span><strong id="val-latch">-</strong></div>
      </div>
      <h3>4 Insignias de Evidencia Separadas</h3>
      <div class="badges" id="evidence-badges">
        <span class="badge" id="badge-selected">SELECTED_IN_SCAN: -</span>
        <span class="badge" id="badge-applied">DRIVER_APPLIED: -</span>
        <span class="badge" id="badge-observed">PHYSICAL_OBSERVED: -</span>
        <span class="badge" id="badge-persisted">ACTIVE_PERSISTED: -</span>
      </div>
      <h3>Canales de Salida</h3>
      <ul class="channels">
        <li>tower.red <span class="tag phys">PHYSICAL gpiochip0:17</span> <strong id="ch-red">-</strong></li>
        <li>tower.green <span class="tag phys">PHYSICAL gpiochip0:18</span> <strong id="ch-green">-</strong></li>
        <li>tower.blue <span class="tag virt">VIRTUAL (Sin GPIO cableado)</span> <strong id="ch-blue">-</strong></li>
      </ul>
      <div class="actions">
        <button id="btn-rearm" type="button">Rearmar Control</button>
        <button id="btn-rollback" type="button" class="warn-btn">Rollback a Anterior</button>
        <button id="btn-manual-off" type="button">Manual Seguro OFF (TTL 10s)</button>
      </div>
    </section>

    <section class="card">
      <h2>Comandos en Espanol / Voz Push-to-Talk (es-PE)</h2>
      <p class="muted">El asistente nunca auto-activa: primero normaliza y exige revision explicita + Authorize + Activate.</p>
      <textarea id="spanish-input" rows="3" placeholder="Ej: Cambia el retardo de 10 segundos a 5 segundos"></textarea>
      <div class="actions">
        <button id="btn-ptt" type="button">Push-to-Talk (es-PE)</button>
        <button id="btn-parse-es" type="button">Interpretar Intencion</button>
      </div>
      <pre id="intent-preview" class="codebox"></pre>
    </section>

    <section class="card">
      <h2>Diagrama Ladder Vivo (Proyeccion Determinista)</h2>
      <div id="ladder-container" class="svg-wrap"></div>
    </section>

    <section class="card">
      <h2>Flujo Transaccional &amp; Importador IEC (ST / PLCopen LD)</h2>
      <textarea id="pkg-json-input" rows="5" placeholder="Pegue JSON firmado ModulePackageV1 o inspeccione propuestas"></textarea>
      <div class="row">
        <input id="tx-id-input" value="tx-console-1" aria-label="Transaction ID">
        <input id="pkg-hash-input" placeholder="expectedPackageHash" aria-label="Expected Package Hash">
      </div>
      <div class="actions">
        <button id="btn-propose" type="button">1. Proponer (Shadow)</button>
        <button id="btn-authorize" type="button">2. Autorizar</button>
        <button id="btn-activate" type="button">3. Activar en Frontera</button>
        <button id="btn-cancel" type="button">Cancelar Armed</button>
      </div>
      <pre id="tx-log" class="codebox"></pre>
    </section>
  </main>

  <script src="/console/app.js"></script>
</body>
</html>`;
}

export function renderConsoleCss(): string {
  return `:root{--bg:#0b1120;--card:#111827;--border:#1f2937;--fg:#f1f5f9;--muted:#94a3b8;--accent:#38bdf8;--ok:#10b981;--warn:#f59e0b;--bad:#ef4444;}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font-family:system-ui,-apple-system,sans-serif}
.topbar{display:flex;justify-content:space-between;align-items:center;padding:12px 20px;border-bottom:1px solid var(--border);background:#0f172a;gap:12px;flex-wrap:wrap}
h1{font-size:18px;margin:0}h2{font-size:15px;margin:0 0 10px}h3{font-size:13px;color:var(--muted);margin:12px 0 6px}
.muted{color:var(--muted);font-size:12px}.auth-bar,.actions,.row{display:flex;gap:8px;flex-wrap:wrap;margin-top:8px}
input,select,textarea{background:#0f172a;color:var(--fg);border:1px solid #334155;border-radius:6px;padding:6px 8px;font-family:inherit;width:100%}
.auth-bar input,.auth-bar select,.row input{width:auto;min-width:140px}
button{background:#1d4ed8;color:#fff;border:none;border-radius:6px;padding:8px 12px;cursor:pointer;font-weight:600}
button:hover{filter:brightness(1.1)}.warn-btn{background:#b45309}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(360px,1fr));gap:14px;padding:16px}
.card{background:var(--card);border:1px solid var(--border);border-radius:10px;padding:14px}
.kv-grid{display:grid;grid-template-columns:1fr 1fr;gap:8px}.kv-grid div{background:#0f172a;padding:8px;border-radius:6px}
.kv-grid span{display:block;font-size:11px;color:var(--muted)}
.badges{display:flex;gap:6px;flex-wrap:wrap}.badge{padding:4px 8px;border-radius:999px;font-size:11px;background:#1e293b;border:1px solid #334155}
.badge.ok{border-color:var(--ok);color:#6ee7b7}.badge.no{border-color:var(--bad);color:#fca5a5}
.channels{list-style:none;padding:0;margin:0}.channels li{display:flex;justify-content:space-between;align-items:center;padding:6px 0;border-bottom:1px solid var(--border)}
.tag{font-size:10px;padding:2px 6px;border-radius:4px;font-weight:700}.tag.phys{background:#064e3b;color:#6ee7b7}.tag.virt{background:#312e81;color:#c7d2fe}
.banner{padding:8px 10px;border-radius:6px;background:#1e3a8a;margin-bottom:8px;font-size:12px}.banner.warn{background:#78350f;color:#fde68a}.hidden{display:none}
.codebox{background:#020617;border:1px solid var(--border);padding:8px;border-radius:6px;max-height:160px;overflow:auto;font-size:12px}
.svg-wrap svg{max-width:100%;height:auto;border-radius:6px}`;
}

export function renderConsoleJs(): string {
  return `(function(){
  const $ = (id) => document.getElementById(id);
  const TX_KEY = "plc_auto_pending_tx";
  const TERMINAL_TX_STATES = new Set(["STABLE", "ACTIVE_VOLATILE", "REJECTED", "SUPERSEDED", "ROLLED_BACK", "RECOVERY_REQUIRED"]);
  function escXml(s){
    return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#39;");
  }
  function headers(){ return { "Authorization": "Bearer " + $("role-token").value, "Content-Type": "application/json" }; }
  function devId(){ return encodeURIComponent($("device-id").value.trim() || "lyra-bench-01"); }
  async function api(method, path, body){
    const res = await fetch(path, { method, headers: headers(), body: body ? JSON.stringify(body) : undefined });
    const txt = await res.text();
    let json = {};
    try { json = txt ? JSON.parse(txt) : {}; } catch(_) { json = { raw: txt }; }
    if(!res.ok) throw new Error("HTTP " + res.status + ": " + JSON.stringify(json));
    return json;
  }
  function savePending(tx){ localStorage.setItem(TX_KEY, JSON.stringify(tx)); }
  function clearPending(){ localStorage.removeItem(TX_KEY); $("pending-banner").classList.add("hidden"); }
  async function checkPending(){
    const raw = localStorage.getItem(TX_KEY);
    if(!raw) return;
    const p = JSON.parse(raw);
    const b = $("pending-banner");
    b.classList.remove("hidden");
    b.textContent = "Verificando estado tras reconexion para txId=" + p.txId + "...";
    try {
      const prop = await api("GET", "/v1/proposals/" + encodeURIComponent(p.txId));
      const st = (prop.transaction && prop.transaction.revision) ? prop.transaction.revision.state : "UNKNOWN";
      b.textContent = "Reconciliado txId=" + p.txId + " estado=" + st + " revActiva=" + prop.activeRevision;
      if(TERMINAL_TX_STATES.has(st)){ clearPending(); }
    } catch(e){
      b.textContent = "txId=" + p.txId + ": " + e.message;
    }
  }
  function renderLadder(ruleSet){
    if(!ruleSet || !Array.isArray(ruleSet.changes)){ $("ladder-container").textContent = "Sin modulo activo"; return; }
    const rows = ruleSet.changes.filter(r => r.op !== "removeRule");
    const h = Math.max(100, 40 + rows.length * 64);
    let svg = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 640 ' + h + '" style="background:#0f172a;font-family:monospace">';
    svg += '<line x1="16" y1="12" x2="16" y2="' + (h-12) + '" stroke="#64748b" stroke-width="3"/>';
    svg += '<line x1="624" y1="12" x2="624" y2="' + (h-12) + '" stroke="#64748b" stroke-width="3"/>';
    rows.forEach((r, idx) => {
      const y = 44 + idx * 64;
      const condRaw = r.triggerAtOrAbove != null ? (String(r.input || "") + " >= " + (Number(r.triggerAtOrAbove)/10).toFixed(1) + "C") : String(r.input || "");
      const virt = r.output === "tower.blue" ? " (VIRTUAL)" : "";
      svg += '<line x1="16" y1="'+y+'" x2="624" y2="'+y+'" stroke="#38bdf8" stroke-width="2"/>';
      svg += '<rect x="44" y="'+(y-14)+'" width="180" height="28" rx="4" fill="#1e293b" stroke="#38bdf8"/>';
      svg += '<text x="134" y="'+(y+4)+'" fill="#f8fafc" font-size="11" text-anchor="middle">--[ ]-- '+escXml(condRaw)+'</text>';
      svg += '<rect x="250" y="'+(y-16)+'" width="150" height="32" rx="4" fill="#1e293b" stroke="#fbbf24"/>';
      svg += '<text x="325" y="'+(y+4)+'" fill="#fbbf24" font-size="11" text-anchor="middle">'+escXml(r.blockType||"BLOCK")+' '+escXml(r.holdMs||0)+'ms</text>';
      svg += '<rect x="430" y="'+(y-16)+'" width="175" height="32" rx="16" fill="#1e293b" stroke="#34d399"/>';
      svg += '<text x="517" y="'+(y+4)+'" fill="#34d399" font-size="11" text-anchor="middle">( ) '+escXml(r.output||"")+virt+'</text>';
    });
    svg += '</svg>';
    $("ladder-container").innerHTML = svg;
  }
  async function refresh(){
    try {
      const st = await api("GET", "/v1/devices/" + devId() + "/state");
      $("conn-status").textContent = "Conectado · device=" + st.deviceId + " · ok";
      $("val-rev").textContent = st.activeRevision;
      $("val-armed").textContent = String(st.controlArmed);
      $("val-scan").textContent = st.scanId + " / " + st.bootId;
      $("val-latch").textContent = String(st.fallbackLatched);
      const ev = st.lastEvidence;
      if(ev){
        const appliedOk = ev.driverApplied === "APPLIED";
        const physicalObserved = ev.physicalObservedMask !== null && ev.physicalObservedMask !== undefined;
        const witnessDiscrepancy = physicalObserved && ev.physicalObservedMask !== (ev.appliedMask & 3);
        $("badge-selected").textContent = "SELECTED_IN_SCAN: " + ev.selectedInScan + " (mask " + ev.desiredMask + ")";
        $("badge-selected").className = "badge " + (ev.selectedInScan ? "ok" : "no");
        $("badge-applied").textContent = "DRIVER_APPLIED: " + ev.driverApplied + " (mask " + ev.appliedMask + ")";
        $("badge-applied").className = "badge " + (appliedOk ? "ok" : "no");
        $("badge-observed").textContent = "PHYSICAL_OBSERVED: " + physicalObserved + " (" + ev.physicalObservedMask + ")";
        $("badge-observed").className = "badge " + (physicalObserved ? "ok" : "no");
        $("badge-persisted").textContent = "ACTIVE_PERSISTED: " + ev.activePersisted;
        $("badge-persisted").className = "badge " + (ev.activePersisted ? "ok" : "no");
        const wb = $("witness-banner");
        if(witnessDiscrepancy){ wb.classList.remove("hidden"); wb.textContent = "DISCREPANCIA TESTIGO: applied=" + ev.appliedMask + " observed=" + ev.physicalObservedMask + " guard=" + ev.guardReason; }
        else { wb.classList.add("hidden"); }
        $("ch-red").textContent = (ev.appliedMask & 1) ? "ON" : "OFF";
        $("ch-green").textContent = (ev.appliedMask & 2) ? "ON" : "OFF";
        $("ch-blue").textContent = (ev.appliedMask & 4) ? "ON (VIRTUAL)" : "OFF";
      }
      renderLadder(st.activeRuleSet);
      await checkPending();
    } catch(e){
      $("conn-status").textContent = "Desconectado (" + e.message + ")";
    }
  }
  function normalizeEsClient(raw){
    const tens = {veinte:20,treinta:30,cuarenta:40,cincuenta:50,sesenta:60,setenta:70,ochenta:80,noventa:90};
    const units = {cero:0,uno:1,una:1,dos:2,tres:3,cuatro:4,cinco:5,seis:6,siete:7,ocho:8,nueve:9,diez:10,once:11,doce:12,quince:15,veinte:20};
    let s = String(raw || "").toLowerCase().normalize("NFD").replace(/[\u0300-\u036f]/g,"")
      .replace(/°\s*c\b/g," c ").replace(/°/g," ").replace(/\bcelcius\b/g,"celsius")
      .replace(/\bgrados?\s+centigrados?\b/g,"c").replace(/\bgrados?\s+celsius\b/g,"c")
      .replace(/\bgrados?(?!\s*(?:fahrenheit|kelvin|f\b|k\b))\b/g,"c")
      .replace(/[¿?¡!,;:()"]/g," ").replace(/\s+/g," ").trim();
    const toks = s.split(" "), out = [];
    for(let i=0;i<toks.length;i++){
      const t = toks[i];
      if(t in tens && toks[i+1]==="y" && (toks[i+2] in units) && units[toks[i+2]]<10){ out.push(String(tens[t]+units[toks[i+2]])); i+=2; }
      else if(t in units){ out.push(String(units[t])); }
      else { out.push(t); }
    }
    return out.join(" ").replace(/\b(\d+)\s+(?:punto|coma)\s+(\d+)\b/g,"$1.$2");
  }
  function preflightEs(raw){
    const norm = normalizeEsClient(raw);
    if(norm.length < 4) return { outcome: "CLARIFY", norm, message: "Orden incompleta." };
    if(/\b(ignora\w*|desactiva\w*|anula\w*|bypassea\w*|bypass)\b.*\b(calidad|interlock|seguridad|shield|inhibidor)\b/.test(norm) || /\b\d+(?:\.\d+)?\s*(?:grados?\s+)?(f|fahrenheit|kelvin|psi|bar|rpm|volts?|amperios?)\b/.test(norm)){
      return { outcome: "REJECT", norm, message: "Rechazado: unidad incompatible o intento de saltar proteccion." };
    }
    const ch = /\bazul|blue\b/.test(norm) ? "tower.blue (VIRTUAL)" : (/\bverde|green\b/.test(norm) ? "tower.green" : (/\broja|rojo|red\b/.test(norm) ? "tower.red" : null));
    return { outcome: ch ? "ACCEPT_PREVIEW" : "CLARIFY", norm, targetChannel: ch, requiresConfirmation: Boolean(ch), message: ch ? "Vista previa determinista lista para revision -> Proponer -> Autorizar -> Activar (nunca auto-activa)." : "Especifique canal objetivo (roja, verde o azul virtual)." };
  }
  $("btn-refresh").onclick = refresh;
  $("btn-rearm").onclick = async () => { $("tx-log").textContent = JSON.stringify(await api("POST", "/v1/devices/"+devId()+"/rearm", {}), null, 2); refresh(); };
  $("btn-manual-off").onclick = async () => { $("tx-log").textContent = JSON.stringify(await api("POST", "/v1/devices/"+devId()+"/manual", {mask:0,validMask:7,ttlMs:10000}), null, 2); refresh(); };
  $("btn-rollback").onclick = async () => {
    const rev = $("val-rev").textContent || "1";
    const txId = "rb-" + Date.now();
    savePending({ txId });
    $("tx-log").textContent = JSON.stringify(await api("POST", "/v1/devices/"+devId()+"/rollbacks", { txId, baseRevision: rev, reason: "console-rollback" }), null, 2);
    refresh();
  };
  $("btn-propose").onclick = async () => {
    const txId = $("tx-id-input").value.trim();
    const pkg = JSON.parse($("pkg-json-input").value);
    savePending({ txId });
    const res = await api("POST", "/v1/devices/"+devId()+"/proposals", { txId, package: pkg });
    if(res.revision && res.revision.packageHash) $("pkg-hash-input").value = res.revision.packageHash;
    $("tx-log").textContent = JSON.stringify(res, null, 2);
  };
  $("btn-authorize").onclick = async () => {
    const txId = $("tx-id-input").value.trim();
    const h = $("pkg-hash-input").value.trim();
    $("tx-log").textContent = JSON.stringify(await api("POST", "/v1/proposals/"+encodeURIComponent(txId)+"/authorizations", { operatorId: "op-web", expectedPackageHash: h, ttlMs: 60000 }), null, 2);
  };
  $("btn-activate").onclick = async () => {
    const txId = $("tx-id-input").value.trim();
    savePending({ txId });
    const res = await api("POST", "/v1/proposals/"+encodeURIComponent(txId)+"/activate", {});
    $("tx-log").textContent = JSON.stringify(res, null, 2);
    const st = (res.revision && res.revision.state) ? res.revision.state : "";
    if(TERMINAL_TX_STATES.has(st)) clearPending();
    refresh();
  };
  $("btn-cancel").onclick = async () => {
    const txId = $("tx-id-input").value.trim();
    $("tx-log").textContent = JSON.stringify(await api("POST", "/v1/proposals/"+encodeURIComponent(txId)+"/cancel", {}), null, 2);
    clearPending();
  };
  $("btn-parse-es").onclick = () => {
    $("intent-preview").textContent = JSON.stringify(preflightEs($("spanish-input").value), null, 2);
  };
  $("btn-ptt").onclick = () => {
    const SR = window.SpeechRecognition || window.webkitSpeechRecognition;
    if(!SR){ $("intent-preview").textContent = "SpeechRecognition no disponible en este navegador; use entrada de texto o iPad app."; return; }
    const rec = new SR();
    rec.lang = "es-PE";
    rec.interimResults = false;
    rec.onresult = (ev) => { $("spanish-input").value = ev.results[0][0].transcript; };
    rec.start();
  };
  refresh();
})();`;
}
