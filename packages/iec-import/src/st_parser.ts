import { type RuleOp, type RuleSetV1 } from "@plc-auto/control-ir";
import type { LadderDiagramModel, LadderRung } from "./ladder_svg.js";

export class StDiagnosticError extends Error {
  constructor(
    readonly line: number,
    readonly col: number,
    message: string
  ) {
    super(`ST(${line}:${col}): ${message}`);
    this.name = "StDiagnosticError";
  }
}

interface Token {
  kind: "IDENT" | "NUMBER" | "TIME" | "STRING" | "OP" | "PUNCT" | "EOF";
  text: string;
  line: number;
  col: number;
}

const ALLOWED_TYPES = new Set(["BOOL", "INT", "DINT", "TIME", "TON", "TOF", "TP", "CTU", "R_TRIG", "F_TRIG"]);
const FORBIDDEN_KEYWORDS = new Set(["WHILE", "FOR", "REPEAT", "GOTO", "POINTER", "REF_TO", "ADR", "SIZEOF"]);

function parseTimeLiteralMs(raw: string, line: number, col: number): number {
  const m = raw.match(/^T#(\d+(?:\.\d+)?)(ms|s|m)$/i);
  if (!m) {
    throw new StDiagnosticError(line, col, `Invalid IEC TIME literal '${raw}'`);
  }
  const n = Number(m[1]);
  const unit = m[2]!.toLowerCase();
  if (unit === "ms") return Math.round(n);
  if (unit === "s") return Math.round(n * 1000);
  return Math.round(n * 60_000);
}

function tokenizeSt(source: string): Token[] {
  const tokens: Token[] = [];
  let line = 1;
  let col = 1;
  let i = 0;

  while (i < source.length) {
    const ch = source[i]!;
    if (ch === "\n") {
      line++;
      col = 1;
      i++;
      continue;
    }
    if (/\s/.test(ch)) {
      col++;
      i++;
      continue;
    }
    // Comments (* ... *) or // ...
    if (ch === "(" && source[i + 1] === "*") {
      const startL = line;
      const startC = col;
      i += 2;
      col += 2;
      while (i < source.length && !(source[i] === "*" && source[i + 1] === ")")) {
        if (source[i] === "\n") {
          line++;
          col = 1;
        } else {
          col++;
        }
        i++;
      }
      if (i >= source.length) {
        throw new StDiagnosticError(startL, startC, "Unterminated (* ... *) comment");
      }
      i += 2;
      col += 2;
      continue;
    }
    if (ch === "/" && source[i + 1] === "/") {
      while (i < source.length && source[i] !== "\n") {
        i++;
        col++;
      }
      continue;
    }

    // Reject raw physical I/O addresses %I, %Q, %M (I02)
    if (ch === "%") {
      const startC = col;
      let j = i + 1;
      while (j < source.length && /[A-Za-z0-9_.]/.test(source[j]!)) j++;
      const addr = source.slice(i, j);
      throw new StDiagnosticError(
        line,
        startC,
        `Direct physical I/O address '${addr}' is forbidden; bind symbolic site-profile names only.`
      );
    }

    // Two-char operators
    const two = source.slice(i, i + 2);
    if ([":=", ">=", "<=", "<>", "=>"].includes(two)) {
      tokens.push({ kind: "OP", text: two, line, col });
      i += 2;
      col += 2;
      continue;
    }

    if ("+-*/=<>".includes(ch)) {
      tokens.push({ kind: "OP", text: ch, line, col });
      i++;
      col++;
      continue;
    }

    if ("(),;:.".includes(ch)) {
      tokens.push({ kind: "PUNCT", text: ch, line, col });
      i++;
      col++;
      continue;
    }

    // Time literal T#10s / t#500ms
    if ((ch === "T" || ch === "t") && source[i + 1] === "#") {
      const startC = col;
      let j = i + 2;
      while (j < source.length && /[A-Za-z0-9_.]/.test(source[j]!)) j++;
      const lit = source.slice(i, j);
      tokens.push({ kind: "TIME", text: lit, line, col: startC });
      col += lit.length;
      i = j;
      continue;
    }

    if (/[0-9]/.test(ch)) {
      const startC = col;
      let j = i;
      while (j < source.length && /[0-9.]/.test(source[j]!)) j++;
      const num = source.slice(i, j);
      tokens.push({ kind: "NUMBER", text: num, line, col: startC });
      col += num.length;
      i = j;
      continue;
    }

    if (/[A-Za-z_]/.test(ch)) {
      const startC = col;
      let j = i;
      while (j < source.length && /[A-Za-z0-9_.]/.test(source[j]!)) j++;
      const ident = source.slice(i, j);
      const upper = ident.toUpperCase();
      if (FORBIDDEN_KEYWORDS.has(upper)) {
        throw new StDiagnosticError(
          line,
          startC,
          `Unbounded control/unsafe construct '${ident}' is forbidden in bounded scan subset.`
        );
      }
      tokens.push({ kind: "IDENT", text: ident, line, col: startC });
      col += ident.length;
      i = j;
      continue;
    }

    throw new StDiagnosticError(line, col, `Unexpected character '${ch}'`);
  }

  tokens.push({ kind: "EOF", text: "", line, col });
  return tokens;
}

function normalizeSymbol(name: string): string {
  return name.replace(/_/g, ".");
}

function canonicalOutputName(sym: string, line: number, col: number): string {
  const n = normalizeSymbol(sym).toLowerCase();
  if (n === "tower.red" || n === "tower.green" || n === "tower.blue") return n;
  throw new StDiagnosticError(
    line,
    col,
    `Unknown output symbol '${sym}' (allowed: tower_red/tower.red, tower_green, tower_blue)`
  );
}

function canonicalInputName(sym: string, line: number, col: number): string {
  const n = normalizeSymbol(sym).toLowerCase();
  if (n === "temp.process" || n === "start.stop" || n === "physical.inhibit") return n;
  throw new StDiagnosticError(
    line,
    col,
    `Unknown input symbol '${sym}' (allowed: temp_process/temp.process, start_stop)`
  );
}

export interface StImportResult {
  programName: string;
  ruleSet: RuleSetV1;
  ladder: LadderDiagramModel;
  declaredVars: Record<string, string>;
}

/**
 * Parses and lowers our bounded IEC 61131-3 ST subset cleanly into RuleSetV1 + LadderDiagramModel.
 */
export function importStructuredText(
  source: string,
  baseRevision = "1"
): StImportResult {
  const tokens = tokenizeSt(source);
  let pos = 0;

  const peek = () => tokens[pos]!;
  const next = () => tokens[pos++]!;
  const expectIdent = (kw?: string) => {
    const t = peek();
    if (t.kind !== "IDENT" || (kw && t.text.toUpperCase() !== kw)) {
      throw new StDiagnosticError(
        t.line,
        t.col,
        kw ? `Expected keyword '${kw}', got '${t.text}'` : `Expected identifier, got '${t.text}'`
      );
    }
    return next();
  };
  const expectText = (txt: string) => {
    const t = peek();
    if (t.text !== txt) {
      throw new StDiagnosticError(t.line, t.col, `Expected '${txt}', got '${t.text}'`);
    }
    return next();
  };

  expectIdent("PROGRAM");
  const progTok = expectIdent();
  const programName = progTok.text;

  const declaredVars: Record<string, string> = {};
  if (peek().kind === "IDENT" && peek().text.toUpperCase() === "VAR") {
    next();
    while (!(peek().kind === "IDENT" && peek().text.toUpperCase() === "END_VAR")) {
      if (peek().kind === "EOF") {
        throw new StDiagnosticError(peek().line, peek().col, "Expected END_VAR");
      }
      const vName = expectIdent();
      expectText(":");
      const vType = expectIdent();
      const typeUpper = vType.text.toUpperCase();
      if (!ALLOWED_TYPES.has(typeUpper)) {
        throw new StDiagnosticError(vType.line, vType.col, `Unsupported VAR type '${vType.text}'`);
      }
      declaredVars[vName.text] = typeUpper;
      expectText(";");
    }
    expectIdent("END_VAR");
  }

  const fbStates = new Map<
    string,
    {
      fbType: string;
      input: string;
      trig?: number;
      bandLow?: number;
      bandHigh?: number;
      ptMs?: number;
      pvCount?: number;
      line: number;
      col: number;
    }
  >();

  const assignedOutputs = new Map<string, { line: number; col: number }>();
  const changes: RuleOp[] = [];
  const rungs: LadderRung[] = [];

  while (!(peek().kind === "IDENT" && peek().text.toUpperCase() === "END_PROGRAM")) {
    const cur = peek();
    if (cur.kind === "EOF") {
      throw new StDiagnosticError(cur.line, cur.col, "Missing END_PROGRAM");
    }

    // FB call: fbName(IN := expr, PT := T#10s);
    if (cur.kind === "IDENT" && tokens[pos + 1]?.text === "(") {
      const fbNameTok = next();
      const fbType = declaredVars[fbNameTok.text];
      if (!fbType || !["TON", "TOF", "TP", "CTU"].includes(fbType)) {
        throw new StDiagnosticError(
          fbNameTok.line,
          fbNameTok.col,
          `Unknown or forbidden function/FB call '${fbNameTok.text}'`
        );
      }
      expectText("(");
      let input = "temp.process";
      let trig: number | undefined;
      let bandLow: number | undefined;
      let bandHigh: number | undefined;
      let ptMs: number | undefined;
      let pvCount: number | undefined;

      while (peek().text !== ")") {
        const param = expectIdent();
        expectText(":=");
        const paramUp = param.text.toUpperCase();
        if (paramUp === "PT") {
          const t = next();
          if (t.kind !== "TIME") {
            throw new StDiagnosticError(t.line, t.col, "Expected TIME literal for PT (e.g. T#10s)");
          }
          ptMs = parseTimeLiteralMs(t.text, t.line, t.col);
        } else if (paramUp === "PV") {
          const n = next();
          if (n.kind !== "NUMBER") {
            throw new StDiagnosticError(n.line, n.col, "Expected integer PV");
          }
          pvCount = Number(n.text);
        } else if (paramUp === "IN" || paramUp === "CU") {
          // Parse simple comparison `temp_process >= 450` or band `temp_process >= 400 AND temp_process < 450`
          const sigTok = expectIdent();
          input = canonicalInputName(sigTok.text, sigTok.line, sigTok.col);
          if ([">=", ">", "<=", "<"].includes(peek().text)) {
            const op = next();
            const valTok = next();
            if (valTok.kind !== "NUMBER") {
              throw new StDiagnosticError(valTok.line, valTok.col, "Expected numeric threshold");
            }
            const v = Number(valTok.text);
            if (op.text === ">=" || op.text === ">") {
              trig = v;
              bandLow = v;
            } else {
              bandHigh = v;
            }
            if (peek().kind === "IDENT" && peek().text.toUpperCase() === "AND") {
              next();
              const sig2 = expectIdent();
              canonicalInputName(sig2.text, sig2.line, sig2.col);
              const op2 = next();
              const v2Tok = next();
              if (v2Tok.kind !== "NUMBER") {
                throw new StDiagnosticError(v2Tok.line, v2Tok.col, "Expected numeric threshold");
              }
              if (op2.text === "<" || op2.text === "<=") bandHigh = Number(v2Tok.text);
            }
          }
        } else {
          throw new StDiagnosticError(param.line, param.col, `Unsupported FB parameter '${param.text}'`);
        }
        if (peek().text === ",") next();
      }
      expectText(")");
      expectText(";");
      const fbEntry: {
        fbType: string;
        input: string;
        trig?: number;
        bandLow?: number;
        bandHigh?: number;
        ptMs?: number;
        pvCount?: number;
        line: number;
        col: number;
      } = {
        fbType,
        input,
        line: fbNameTok.line,
        col: fbNameTok.col,
      };
      if (trig !== undefined) fbEntry.trig = trig;
      if (bandLow !== undefined) fbEntry.bandLow = bandLow;
      if (bandHigh !== undefined) fbEntry.bandHigh = bandHigh;
      if (ptMs !== undefined) fbEntry.ptMs = ptMs;
      if (pvCount !== undefined) fbEntry.pvCount = pvCount;
      fbStates.set(fbNameTok.text, fbEntry);
      continue;
    }

    // Direct assignment: tower_red := fb.Q; OR tower_green := start_stop;
    if (cur.kind === "IDENT" && tokens[pos + 1]?.text === ":=") {
      const lhsTok = next();
      const outName = canonicalOutputName(lhsTok.text, lhsTok.line, lhsTok.col);
      if (assignedOutputs.has(outName)) {
        throw new StDiagnosticError(
          lhsTok.line,
          lhsTok.col,
          `Duplicate coil write to output '${outName}' (single-writer violation)`
        );
      }
      assignedOutputs.set(outName, { line: lhsTok.line, col: lhsTok.col });
      next(); // :=

      const rhsTok = expectIdent();
      let maybeFbName: string | undefined;
      let maybeMember: string | undefined;
      if (rhsTok.text.includes(".")) {
        const dotIdx = rhsTok.text.indexOf(".");
        const head = rhsTok.text.slice(0, dotIdx);
        const tail = rhsTok.text.slice(dotIdx + 1);
        if (fbStates.has(head)) {
          maybeFbName = head;
          maybeMember = tail;
        }
      } else if (peek().text === ".") {
        next();
        const memTok = expectIdent();
        maybeFbName = rhsTok.text;
        maybeMember = memTok.text;
      }
      if (maybeFbName !== undefined) {
        if ((maybeMember ?? "").toUpperCase() !== "Q") {
          throw new StDiagnosticError(rhsTok.line, rhsTok.col, "Expected '.Q' FB output field");
        }
        const fb = fbStates.get(maybeFbName);
        if (!fb) {
          throw new StDiagnosticError(rhsTok.line, rhsTok.col, `FB instance '${maybeFbName}' not called yet`);
        }
        expectText(";");

        const ruleId = `${outName}.${fb.fbType.toLowerCase()}`;
        const op: RuleOp =
          fb.bandLow !== undefined && fb.bandHigh !== undefined
            ? {
                op: "upsertRule",
                ruleId: outName === "tower.blue" ? "temp.prealarm-pulse" : ruleId,
                templateId: "bandPrealarmPulse",
                input: fb.input,
                unit: "degC_x10",
                bandLow: fb.bandLow,
                bandHigh: fb.bandHigh,
                pulsePeriodMs: fb.ptMs ?? 2000,
                requiredQuality: "GOOD",
                onInputFault: "COMMAND_OFF_AND_RESET",
                output: outName,
                timingStateOnChange: "RESET",
                stableBlockId: outName === "tower.blue" ? 1002 : 4001 + changes.length,
                blockType: "PULSE_GEN",
              }
            : fb.fbType === "TON"
              ? {
                  op: "upsertRule",
                  ruleId: outName === "tower.red" ? "temp.high-alarm" : ruleId,
                  templateId: "thresholdHoldWithLatchedHysteresis",
                  input: fb.input,
                  unit: fb.input === "start.stop" ? "bool" : "degC_x10",
                  triggerAtOrAbove: fb.trig ?? 450,
                  clearAtOrBelow: fb.trig ?? 450,
                  holdMs: fb.ptMs ?? 10_000,
                  requiredQuality: "GOOD",
                  onInputFault: "COMMAND_OFF_AND_RESET",
                  output: outName,
                  timingStateOnChange: "RESET",
                  stableBlockId: outName === "tower.red" ? 1001 : 4001 + changes.length,
                  blockType: "TON",
                }
              : fb.fbType === "TOF"
                ? {
                    op: "upsertRule",
                    ruleId,
                    templateId: "offDelay",
                    input: fb.input,
                    unit: fb.input === "start.stop" ? "bool" : "degC_x10",
                    triggerAtOrAbove: fb.trig ?? 1,
                    holdMs: fb.ptMs ?? 1000,
                    requiredQuality: "GOOD",
                    onInputFault: "COMMAND_OFF_AND_RESET",
                    output: outName,
                    timingStateOnChange: "RESET",
                    stableBlockId: 4001 + changes.length,
                    blockType: "TOF",
                  }
                : fb.fbType === "TP"
                  ? {
                      op: "upsertRule",
                      ruleId,
                      templateId: "pulseTimer",
                      input: fb.input,
                      unit: fb.input === "start.stop" ? "bool" : "degC_x10",
                      triggerAtOrAbove: fb.trig ?? 1,
                      holdMs: fb.ptMs ?? 1000,
                      requiredQuality: "GOOD",
                      onInputFault: "COMMAND_OFF_AND_RESET",
                      output: outName,
                      timingStateOnChange: "RESET",
                      stableBlockId: 4001 + changes.length,
                      blockType: "TP",
                    }
                  : {
                      op: "upsertRule",
                      ruleId,
                      templateId: "counterTrigger",
                      input: fb.input,
                      unit: fb.input === "start.stop" ? "bool" : "degC_x10",
                      triggerAtOrAbove: fb.trig ?? 1,
                      holdMs: fb.pvCount ?? 3,
                      requiredQuality: "GOOD",
                      onInputFault: "COMMAND_OFF_AND_RESET",
                      output: outName,
                      timingStateOnChange: "PRESERVE_ELAPSED",
                      stableBlockId: 4001 + changes.length,
                      blockType: "CTU",
                    };

        changes.push(op);
        rungs.push({
          rungNumber: rungs.length + 1,
          label: `ST Rung ${rungs.length + 1}: ${fb.fbType}`,
          contacts: [
            {
              label:
                fb.bandLow !== undefined && fb.bandHigh !== undefined
                  ? `${fb.input} in [${fb.bandLow}, ${fb.bandHigh})`
                  : `${fb.input} >= ${fb.trig ?? 1}`,
              negated: false,
            },
          ],
          block: {
            kind: fb.bandLow !== undefined && fb.bandHigh !== undefined ? "PULSE_GEN" : fb.fbType,
            param: fb.fbType === "CTU" ? `PV=${fb.pvCount ?? 0}` : `${fb.ptMs ?? 0}ms`,
          },
          coil: {
            output: outName,
            virtual: outName === "tower.blue",
          },
        });
        continue;
      }

      // Direct boolean pass-through e.g. tower_green := start_stop;
      const inName = canonicalInputName(rhsTok.text, rhsTok.line, rhsTok.col);
      expectText(";");
      changes.push({
        op: "upsertRule",
        ruleId: `${outName}.copy`,
        templateId: "copyBoolean",
        input: inName,
        unit: "bool",
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: outName,
        timingStateOnChange: "RESET",
        stableBlockId: 4100 + changes.length,
        blockType: "THRESHOLD_HYST",
      });
      rungs.push({
        rungNumber: rungs.length + 1,
        label: `ST Rung ${rungs.length + 1}: Copy`,
        contacts: [{ label: inName, negated: false }],
        coil: { output: outName, virtual: outName === "tower.blue" },
      });
      continue;
    }

    // Bounded IF <sig> >= <val> THEN ... END_IF; (supports simple latched hysteresis assignment if present)
    if (cur.kind === "IDENT" && cur.text.toUpperCase() === "IF") {
      next();
      const sigTok = expectIdent();
      const inName = canonicalInputName(sigTok.text, sigTok.line, sigTok.col);
      const cmp = next();
      if (cmp.text !== ">=" && cmp.text !== ">") {
        throw new StDiagnosticError(
          cmp.line,
          cmp.col,
          `Unsupported IF comparator '${cmp.text}' (only '>=' and '>' lower safely)`
        );
      }
      const valTok = next();
      if (valTok.kind !== "NUMBER") {
        throw new StDiagnosticError(valTok.line, valTok.col, "Expected numeric threshold in IF");
      }
      const trig = cmp.text === ">" ? Number(valTok.text) + 1 : Number(valTok.text);
      expectIdent("THEN");
      const outTok = expectIdent();
      const outName = canonicalOutputName(outTok.text, outTok.line, outTok.col);
      if (assignedOutputs.has(outName)) {
        throw new StDiagnosticError(outTok.line, outTok.col, `Duplicate coil write '${outName}'`);
      }
      assignedOutputs.set(outName, { line: outTok.line, col: outTok.col });
      expectText(":=");
      const lit = expectIdent();
      const litUp = lit.text.toUpperCase();
      if (litUp !== "TRUE" && litUp !== "FALSE") {
        throw new StDiagnosticError(lit.line, lit.col, `Expected boolean literal TRUE/FALSE, got '${lit.text}'`);
      }
      if (litUp === "FALSE") {
        throw new StDiagnosticError(
          lit.line,
          lit.col,
          "Inverted IF ... THEN output := FALSE cannot safely lower to active-high threshold rule"
        );
      }
      expectText(";");
      let clearAt = trig;
      if (peek().kind === "IDENT" && peek().text.toUpperCase() === "ELSE") {
        next();
        const elseOut = expectIdent();
        const elseName = canonicalOutputName(elseOut.text, elseOut.line, elseOut.col);
        if (elseName !== outName) {
          throw new StDiagnosticError(
            elseOut.line,
            elseOut.col,
            `ELSE branch writes '${elseName}' instead of '${outName}' (single-writer per rung)`
          );
        }
        expectText(":=");
        const elseLit = expectIdent();
        if (elseLit.text.toUpperCase() !== "FALSE") {
          throw new StDiagnosticError(elseLit.line, elseLit.col, "Expected FALSE in ELSE branch");
        }
        expectText(";");
        clearAt = trig - 1;
      } else {
        clearAt = trig - 1;
      }
      expectIdent("END_IF");
      expectText(";");
      changes.push({
        op: "upsertRule",
        ruleId: `${outName}.if`,
        templateId: "thresholdHoldWithLatchedHysteresis",
        input: inName,
        unit: "degC_x10",
        triggerAtOrAbove: trig,
        clearAtOrBelow: clearAt,
        holdMs: 0,
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: outName,
        timingStateOnChange: "RESET",
        stableBlockId: 4200 + changes.length,
        blockType: "THRESHOLD_HYST",
      });
      rungs.push({
        rungNumber: rungs.length + 1,
        label: `ST Rung ${rungs.length + 1}: IF Threshold`,
        contacts: [{ label: `${inName} ${cmp.text} ${trig}`, negated: false }],
        coil: { output: outName, virtual: outName === "tower.blue" },
      });
      continue;
    }

    throw new StDiagnosticError(cur.line, cur.col, `Unexpected statement starting with '${cur.text}'`);
  }

  expectIdent("END_PROGRAM");

  return {
    programName,
    declaredVars,
    ruleSet: {
      schema: "rule-set/v1",
      baseRevision,
      changes,
    },
    ladder: {
      title: `POU ${programName} (Imported ST -> Ladder)`,
      rungs,
    },
  };
}
