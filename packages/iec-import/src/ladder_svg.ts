import { type RuleSetV1 } from "@plc-auto/control-ir";

export interface LadderContact {
  label: string;
  negated: boolean;
}

export interface LadderBlock {
  kind: string;
  param: string;
}

export interface LadderCoil {
  output: string;
  virtual: boolean;
}

export interface LadderRung {
  rungNumber: number;
  label: string;
  contacts: LadderContact[];
  block?: LadderBlock;
  coil: LadderCoil;
}

export interface LadderDiagramModel {
  title: string;
  rungs: LadderRung[];
}

function escapeXml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

export function ruleSetToLadderModel(
  ruleSet: RuleSetV1,
  title = "Active PLC Ladder Projection"
): LadderDiagramModel {
  const rungs: LadderRung[] = [];
  for (let i = 0; i < ruleSet.changes.length; i++) {
    const r = ruleSet.changes[i]!;
    if (r.op === "removeRule") continue;
    const contactLabel =
      r.templateId === "bandPrealarmPulse"
        ? `${r.input} [${((r.bandLow ?? 400) / 10).toFixed(1)}..${((r.bandHigh ?? 450) / 10).toFixed(1)} C]`
        : r.triggerAtOrAbove !== undefined && r.triggerAtOrAbove !== null
          ? `${r.input} >= ${(r.triggerAtOrAbove / 10).toFixed(1)} C`
          : `${r.input} = TRUE`;
    const rung: LadderRung = {
      rungNumber: rungs.length + 1,
      label: r.ruleId,
      contacts: [{ label: contactLabel, negated: false }],
      coil: {
        output: r.output,
        virtual: r.output === "tower.blue",
      },
    };
    if (r.holdMs !== undefined && r.holdMs !== null && r.holdMs > 0) {
      rung.block = {
        kind: r.blockType ?? "TON",
        param: `PT=${(r.holdMs / 1000).toFixed(1)}s (${r.timingStateOnChange})`,
      };
    } else if (r.pulsePeriodMs !== undefined && r.pulsePeriodMs !== null) {
      rung.block = {
        kind: r.blockType ?? "PULSE_GEN",
        param: `T=${(r.pulsePeriodMs / 1000).toFixed(1)}s`,
      };
    }
    rungs.push(rung);
  }
  return { title, rungs };
}

/**
 * Renders a deterministic offline SVG representation of a Ladder diagram (§6.5).
 */
export function renderLadderSvg(model: LadderDiagramModel): string {
  const rowH = 92;
  const width = 760;
  const height = Math.max(120, 48 + model.rungs.length * rowH);
  const parts: string[] = [
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${width} ${height}" width="${width}" height="${height}" style="background:#0f172a;font-family:monospace">`,
    `<text x="16" y="24" fill="#e2e8f0" font-size="14" font-weight="bold">${escapeXml(model.title)}</text>`,
    `<line x1="20" y1="36" x2="20" y2="${height - 12}" stroke="#64748b" stroke-width="3"/>`,
    `<line x1="${width - 20}" y1="36" x2="${width - 20}" y2="${height - 12}" stroke="#64748b" stroke-width="3"/>`,
  ];

  for (let i = 0; i < model.rungs.length; i++) {
    const rung = model.rungs[i]!;
    const y = 68 + i * rowH;
    parts.push(`<line x1="20" y1="${y}" x2="${width - 20}" y2="${y}" stroke="#38bdf8" stroke-width="2"/>`);
    parts.push(`<text x="28" y="${y - 20}" fill="#94a3b8" font-size="11">R${rung.rungNumber}: ${escapeXml(rung.label)}</text>`);

    // Contact symbol
    const cLabel = rung.contacts[0]?.label ?? "IN";
    const sym = rung.contacts[0]?.negated ? "--[/]--" : "--[ ]--";
    parts.push(`<rect x="52" y="${y - 16}" width="180" height="32" rx="4" fill="#1e293b" stroke="#38bdf8"/>`);
    parts.push(`<text x="142" y="${y + 4}" fill="#f8fafc" font-size="11" text-anchor="middle">${escapeXml(`${sym} ${cLabel}`)}</text>`);

    // Optional FB block (TON/TOF/TP/CTU/PULSE_GEN)
    if (rung.block) {
      parts.push(`<rect x="280" y="${y - 24}" width="210" height="48" rx="6" fill="#1e293b" stroke="#fbbf24" stroke-width="2"/>`);
      parts.push(`<text x="385" y="${y - 4}" fill="#fbbf24" font-size="12" font-weight="bold" text-anchor="middle">${escapeXml(rung.block.kind)}</text>`);
      parts.push(`<text x="385" y="${y + 14}" fill="#cbd5e1" font-size="11" text-anchor="middle">${escapeXml(rung.block.param)}</text>`);
    }

    // Coil symbol (marking VIRTUAL clearly for tower.blue)
    const coilColor = rung.coil.virtual ? "#818cf8" : "#34d399";
    const coilText = rung.coil.virtual ? `${rung.coil.output} (VIRTUAL)` : rung.coil.output;
    parts.push(`<rect x="540" y="${y - 18}" width="185" height="36" rx="18" fill="#1e293b" stroke="${coilColor}" stroke-width="2"/>`);
    parts.push(`<text x="632" y="${y + 4}" fill="${coilColor}" font-size="11" font-weight="bold" text-anchor="middle">( ) ${escapeXml(coilText)}</text>`);
  }

  parts.push(`</svg>`);
  return parts.join("\n");
}
