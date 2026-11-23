(function(){
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
    let s = String(raw || "").toLowerCase().normalize("NFD").replace(/[̀-ͯ]/g,"")
      .replace(/°s*c/g," c ").replace(/°/g," ").replace(/celcius/g,"celsius")
      .replace(/grados?s+centigrados?/g,"c").replace(/grados?s+celsius/g,"c")
      .replace(/grados?(?!s*(?:fahrenheit|kelvin|f|k))/g,"c")
      .replace(/[¿?¡!,;:()"]/g," ").replace(/s+/g," ").trim();
    const toks = s.split(" "), out = [];
    for(let i=0;i<toks.length;i++){
      const t = toks[i];
      if(t in tens && toks[i+1]==="y" && (toks[i+2] in units) && units[toks[i+2]]<10){ out.push(String(tens[t]+units[toks[i+2]])); i+=2; }
      else if(t in units){ out.push(String(units[t])); }
      else { out.push(t); }
    }
    return out.join(" ").replace(/(d+)s+(?:punto|coma)s+(d+)/g,"$1.$2");
  }
  function preflightEs(raw){
    const norm = normalizeEsClient(raw);
    if(norm.length < 4) return { outcome: "CLARIFY", norm, message: "Orden incompleta." };
    if(/(ignoraw*|desactivaw*|anulaw*|bypasseaw*|bypass).*(calidad|interlock|seguridad|shield|inhibidor)/.test(norm) || /d+(?:.d+)?s*(?:grados?s+)?(f|fahrenheit|kelvin|psi|bar|rpm|volts?|amperios?)/.test(norm)){
      return { outcome: "REJECT", norm, message: "Rechazado: unidad incompatible o intento de saltar proteccion." };
    }
    const ch = /azul|blue/.test(norm) ? "tower.blue (VIRTUAL)" : (/verde|green/.test(norm) ? "tower.green" : (/roja|rojo|red/.test(norm) ? "tower.red" : null));
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
})();