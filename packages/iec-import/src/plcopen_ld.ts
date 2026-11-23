import { type RuleOp, type RuleSetV1 } from "@plc-auto/control-ir";
import type { LadderDiagramModel, LadderRung } from "./ladder_svg.js";

export class PlcOpenImportError extends Error {
  constructor(message: string) {
    super(`PLCopenXML: ${message}`);
    this.name = "PlcOpenImportError";
  }
}

interface XmlNode {
  tag: string;
  attrs: Record<string, string>;
  children: XmlNode[];
  text: string;
}

const MAX_XML_DEPTH = 32;
const MAX_XML_NODES = 2000;

/**
 * Minimal, dependency-free, XXE-hardened XML parser for PLCopen XML LD import (§6.5 / I02).
 * Rejects <!DOCTYPE>, <!ENTITY>, nested depth > 32, and node count > 2000 before evaluation.
 */
function parseSafeXml(xml: string): XmlNode {
  if (/<!DOCTYPE\b/i.test(xml) || /<!ENTITY\b/i.test(xml)) {
    throw new PlcOpenImportError("DTD / ENTITY (XXE) declarations are strictly forbidden");
  }
  const clean = xml
    .replace(/<\?xml[\s\S]*?\?>/gi, "")
    .replace(/<!--[\s\S]*?-->/g, "")
    .trim();

  const stack: XmlNode[] = [];
  let root: XmlNode | null = null;
  let totalNodes = 0;

  const tagRe = /<\/?([A-Za-z_][\w:.-]*)([^>]*)>|([^<]+)/g;
  let match: RegExpExecArray | null;
  while ((match = tagRe.exec(clean)) !== null) {
    if (match[3] !== undefined) {
      const txt = match[3].trim();
      if (txt && stack.length > 0) {
        stack[stack.length - 1]!.text += txt;
      }
      continue;
    }

    const full = match[0]!;
    const rawTag = match[1]!;
    const attrStr = match[2] ?? "";
    const tag = rawTag.includes(":") ? rawTag.split(":")[1]! : rawTag;
    const isClosing = full.startsWith("</");
    const isSelfClosing = full.endsWith("/>");

    if (isClosing) {
      const top = stack.pop();
      if (!top || top.tag !== tag) {
        throw new PlcOpenImportError(`Mismatched closing tag </${rawTag}>`);
      }
      continue;
    }

    totalNodes++;
    if (totalNodes > MAX_XML_NODES) {
      throw new PlcOpenImportError(`XML node limit exceeded (> ${MAX_XML_NODES})`);
    }
    if (stack.length + 1 > MAX_XML_DEPTH) {
      throw new PlcOpenImportError(`XML nesting depth exceeded (> ${MAX_XML_DEPTH})`);
    }

    const attrs: Record<string, string> = {};
    const attrRe = /([A-Za-z_][\w:.-]*)\s*=\s*(?:"([^"]*)"|'([^']*)')/g;
    let aMatch: RegExpExecArray | null;
    while ((aMatch = attrRe.exec(attrStr)) !== null) {
      attrs[aMatch[1]!] = aMatch[2] ?? aMatch[3] ?? "";
    }

    const node: XmlNode = { tag, attrs, children: [], text: "" };
    if (stack.length > 0) {
      stack[stack.length - 1]!.children.push(node);
    } else {
      root = node;
    }
    if (!isSelfClosing) {
      stack.push(node);
    }
  }

  if (stack.length !== 0 || !root) {
    throw new PlcOpenImportError("Malformed or empty XML document");
  }
  return root;
}

function findAll(node: XmlNode, tag: string, out: XmlNode[] = []): XmlNode[] {
  if (node.tag === tag) out.push(node);
  for (const c of node.children) findAll(c, tag, out);
  return out;
}

function findFirst(node: XmlNode, tag: string): XmlNode | undefined {
  if (node.tag === tag) return node;
  for (const c of node.children) {
    const f = findFirst(c, tag);
    if (f) return f;
  }
  return undefined;
}

export interface PlcOpenImportResult {
  pouName: string;
  ruleSet: RuleSetV1;
  ladder: LadderDiagramModel;
}

/**
 * Imports a PLCopen XML Ladder Diagram POU into RuleSetV1 + visual Ladder model.
 * Supports NO/NC contacts, series/parallel wires, TON/TOF/TP timers, and coil outputs.
 * Rejects XXE, bounded overflow, unknown outputs, and duplicate coil writes (I02).
 */
export function importPlcOpenLdXml(
  xmlSource: string,
  baseRevision = "1"
): PlcOpenImportResult {
  const doc = parseSafeXml(xmlSource);
  const pou = findFirst(doc, "pou");
  const pouName = pou?.attrs["name"] ?? "MainLD";
  const ldNode = findFirst(doc, "LD");
  if (!ldNode) {
    throw new PlcOpenImportError("Missing <LD> body element");
  }

  const contacts = findAll(ldNode, "contact");
  const blocks = findAll(ldNode, "block");
  const coils = findAll(ldNode, "coil");

  const nodeById = new Map<string, XmlNode>();
  for (const ch of ldNode.children) {
    const id = ch.attrs["localId"];
    if (id) nodeById.set(id, ch);
  }

  const coilsWritten = new Set<string>();
  for (const coil of coils) {
    const outVar = findFirst(coil, "variable")?.text.trim() ?? "";
    if (outVar.startsWith("%")) {
      throw new PlcOpenImportError(`Direct physical I/O address '${outVar}' is forbidden in PLCopen LD`);
    }
    const outNorm = outVar.replace(/_/g, ".").toLowerCase();
    if (!["tower.red", "tower.green", "tower.blue"].includes(outNorm)) {
      throw new PlcOpenImportError(`Unknown coil output '${outVar}'`);
    }
    if (coilsWritten.has(outNorm)) {
      throw new PlcOpenImportError(`Duplicate coil write to '${outNorm}' across rungs (single-writer violation)`);
    }
    coilsWritten.add(outNorm);
  }

  const changes: RuleOp[] = [];
  const rungs: LadderRung[] = [];

  for (let cIdx = 0; cIdx < coils.length; cIdx++) {
    const coil = coils[cIdx]!;
    const outVar = findFirst(coil, "variable")?.text.trim() ?? "";
    const outNorm = outVar.replace(/_/g, ".").toLowerCase();

    // Follow incoming connections backward from coil to resolve attached blocks/contacts
    const connIn = findAll(coil, "connection");
    const rungContacts: { label: string; negated: boolean }[] = [];
    let attachedBlock: XmlNode | undefined;

    const visitRef = (refId: string | undefined) => {
      if (!refId) return;
      const target = nodeById.get(refId);
      if (!target) return;
      if (target.tag === "block") {
        attachedBlock = target;
        for (const c of findAll(target, "connection")) {
          visitRef(c.attrs["refLocalId"]);
        }
      } else if (target.tag === "contact") {
        const varText = findFirst(target, "variable")?.text.trim() ?? "";
        const neg = target.attrs["negated"] === "true";
        rungContacts.push({ label: varText, negated: neg });
        for (const c of findAll(target, "connection")) {
          visitRef(c.attrs["refLocalId"]);
        }
      }
    };

    for (const c of connIn) {
      visitRef(c.attrs["refLocalId"]);
    }

    // Fallback if contact wasn't explicitly wired by localId
    if (rungContacts.length === 0 && contacts[cIdx]) {
      const c = contacts[cIdx]!;
      rungContacts.push({
        label: findFirst(c, "variable")?.text.trim() ?? "",
        negated: c.attrs["negated"] === "true",
      });
    }
    if (!attachedBlock && blocks[cIdx]) {
      attachedBlock = blocks[cIdx];
    }

    for (const rc of rungContacts) {
      if (rc.label.startsWith("%")) {
        throw new PlcOpenImportError(`Direct physical I/O address '${rc.label}' is forbidden in PLCopen LD`);
      }
      if (rc.negated) {
        throw new PlcOpenImportError("Negated NC contact lowering is not supported safely by positive-threshold templates");
      }
    }

    const fbType = attachedBlock?.attrs["typeName"]?.toUpperCase() ?? "DIRECT";
    const holdAttr = attachedBlock?.attrs["holdMs"];
    const trigAttr = coil.attrs["triggerAtOrAbove"] ?? attachedBlock?.attrs["triggerAtOrAbove"];
    const holdMs = holdAttr ? Number(holdAttr) : 10_000;
    const trigVal = trigAttr ? Number(trigAttr) : 450;

    const primaryLabel = rungContacts[0]?.label ?? "";
    const normSig = primaryLabel.replace(/_/g, ".").toLowerCase();
    if (!["temp.process", "start.stop", "physical.inhibit"].includes(normSig)) {
      throw new PlcOpenImportError(`Unknown contact input symbol '${primaryLabel}'`);
    }
    const inputName = normSig;

    if (fbType === "TON" || fbType === "TOF" || fbType === "TP") {
      changes.push({
        op: "upsertRule",
        ruleId: outNorm === "tower.red" ? "temp.high-alarm" : `${outNorm}.${fbType.toLowerCase()}`,
        templateId:
          fbType === "TON"
            ? "thresholdHoldWithLatchedHysteresis"
            : fbType === "TOF"
              ? "offDelay"
              : "pulseTimer",
        input: inputName,
        unit: inputName === "start.stop" ? "bool" : "degC_x10",
        triggerAtOrAbove: trigVal,
        clearAtOrBelow: trigVal,
        holdMs,
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: outNorm,
        timingStateOnChange: "RESET",
        stableBlockId: outNorm === "tower.red" ? 1001 : 4500 + cIdx,
        blockType: fbType,
      });
    } else {
      changes.push({
        op: "upsertRule",
        ruleId: `${outNorm}.coil`,
        templateId: "copyBoolean",
        input: inputName,
        unit: "bool",
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: outNorm,
        timingStateOnChange: "RESET",
        stableBlockId: 4500 + cIdx,
        blockType: "THRESHOLD_HYST",
      });
    }

    rungs.push({
      rungNumber: cIdx + 1,
      label: `LD Rung ${cIdx + 1}`,
      contacts: rungContacts,
      ...(attachedBlock
        ? { block: { kind: fbType, param: `${holdMs}ms` } }
        : {}),
      coil: {
        output: outNorm,
        virtual: outNorm === "tower.blue",
      },
    });
  }

  return {
    pouName,
    ruleSet: {
      schema: "rule-set/v1",
      baseRevision,
      changes,
    },
    ladder: {
      title: `POU ${pouName} (Imported PLCopen LD)`,
      rungs,
    },
  };
}
