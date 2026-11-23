import { type RuleOp, type RuleSetV1, type SiteProfileV1 } from "@plc-auto/control-ir";
import { normalizeSpanishIntent } from "./normalize.js";

export type IntentOutcome = "ACCEPT" | "CLARIFY" | "REJECT";

export type ChannelName = "tower.red" | "tower.green" | "tower.blue";

export interface IntentContext {
  baseRevision?: string;
  activeHoldMs?: number;
  profile?: SiteProfileV1;
}

export interface ParsedIntentResult {
  outcome: IntentOutcome;
  normalizedText: string;
  intentKind: string;
  ruleSet?: RuleSetV1;
  summaryEs: string;
  clarificationQuestion?: string;
  rejectReason?: string;
  virtualChannelWarning: boolean;
  requiresConfirmation: boolean;
}

function detectChannel(norm: string): { channel?: ChannelName; ambiguous?: boolean } {
  const hasRed = /\b(roja|rojo|red|alarma alta)\b/.test(norm);
  const hasGreen = /\b(verde|green)\b/.test(norm);
  const hasBlue = /\b(azul|celeste|blue)\b/.test(norm);
  const count = (hasRed ? 1 : 0) + (hasGreen ? 1 : 0) + (hasBlue ? 1 : 0);
  if (count > 1) return { ambiguous: true };
  if (hasRed) return { channel: "tower.red" };
  if (hasGreen) return { channel: "tower.green" };
  if (hasBlue) return { channel: "tower.blue" };
  return {};
}

function parseDurationMs(text: string): number | undefined {
  const mSec = text.match(/(\d+(?:\.\d+)?)\s*(?:segundos?|seg|s)\b/);
  if (mSec) return Math.round(Number(mSec[1]) * 1000);
  const mMs = text.match(/(\d+(?:\.\d+)?)\s*(?:milisegundos?|ms)\b/);
  if (mMs) return Math.round(Number(mMs[1]));
  const mMin = text.match(/(\d+(?:\.\d+)?)\s*(?:minutos?|min)\b/);
  if (mMin) return Math.round(Number(mMin[1]) * 60_000);
  return undefined;
}

function accept(
  normalizedText: string,
  intentKind: string,
  baseRevision: string,
  changes: RuleOp[],
  summaryEs: string
): ParsedIntentResult {
  const usesVirtual = changes.some((c) => c.output === "tower.blue");
  return {
    outcome: "ACCEPT",
    normalizedText,
    intentKind,
    ruleSet: {
      schema: "rule-set/v1",
      baseRevision,
      changes,
    },
    summaryEs,
    virtualChannelWarning: usesVirtual,
    requiresConfirmation: true,
  };
}

function clarify(normalizedText: string, question: string): ParsedIntentResult {
  return {
    outcome: "CLARIFY",
    normalizedText,
    intentKind: "AMBIGUOUS",
    summaryEs: question,
    clarificationQuestion: question,
    virtualChannelWarning: false,
    requiresConfirmation: false,
  };
}

function reject(normalizedText: string, reason: string): ParsedIntentResult {
  return {
    outcome: "REJECT",
    normalizedText,
    intentKind: "REJECTED",
    summaryEs: reason,
    rejectReason: reason,
    virtualChannelWarning: false,
    requiresConfirmation: false,
  };
}

/**
 * Deterministic Spanish intent parser for Luckfox PLC (§6.4 / V01).
 * Ambiguous inputs ALWAYS return CLARIFY or REJECT (0 silent guessing / auto-commit).
 */
export function parseSpanishIntent(
  rawText: string,
  ctx: IntentContext = {}
): ParsedIntentResult {
  const norm = normalizeSpanishIntent(rawText);
  const baseRevision = ctx.baseRevision ?? "1";

  if (!norm || norm.length < 4) {
    return clarify(norm, "No entendi la orden. Indique accion, senal o luz a modificar.");
  }

  // 1. Safety shield / bypass / unsupported unit checks (STRICT REJECT)
  if (
    /\b(ignora\w*|desactiva|desactivame|desactivarlo|deshabilita\w*|salta\w*|omite\w*|bypassea\w*|bypass|quita\w*|anula\w*|baja\w*)\b.*\b(calidad|sensor malo|interlocks?|seguridad|shield|escudo|inhibidor\w*|dwell|proteccion\w*|limites?)\b/.test(
      norm
    ) ||
    /\baunque\b.*\b(inhibid\w*|malo|fall\w*|desconectad\w*|stale)\b/.test(norm) ||
    /\bsin\s+(shield|escudo|seguridad|confirmacion|interlock|proteccion)\b/.test(norm)
  ) {
    return reject(
      norm,
      "Rechazado: el Reactive Safety Shield, calidad del sensor e interlocks no pueden desactivarse por voz."
    );
  }

  if (/\b(\d+(?:\.\d+)?)\s*(?:grados?\s+)?(f|fahrenheit|kelvin|psi|bar|volts?|amperios?|rpm)\b/.test(norm)) {
    return reject(
      norm,
      "Rechazado: unidad no compatible con el perfil (use grados Celsius 'C' y segundos 's')."
    );
  }

  if (
    /\b(borra el historial|borra todo|formatea|reinicia el kernel|accede a internet|abre un puerto|ssh|wifi|gpio\s*\d+)\b/.test(
      norm
    )
  ) {
    return reject(norm, "Rechazado: comando fuera del alcance permitido por la politica del PLC.");
  }

  // Check out-of-range temperature traps (> 125C or < -40C matching THT-02 site-profile maxValid/minValid)
  const tempNums = [...norm.matchAll(/(-?\d+(?:\.\d+)?)\s*(?:c|celsius)\b/g)].map((m) =>
    Number(m[1])
  );
  if (tempNums.some((v) => v > 125 || v < -40)) {
    return reject(
      norm,
      "Rechazado: temperatura fuera del rango valido del sensor THT-02 (-40 C a 125 C)."
    );
  }

  // Intent 5: Fault fail-safe rule ("Si falla el sensor de temperatura, apaga las salidas")
  if (
    (/\b(si\s+)?(fall\w*|se desconecta\w*|pierd[ea]|malo|invalido|stale)\b.*\b(sensor|temperatura)\b/.test(
      norm
    ) ||
      /\b(sensor|temperatura)\b.*\b(fall\w*|desconecta\w*|pierd[ea]\s*calidad|malo|invalido|stale)\b/.test(
        norm
      )) &&
    /\b(apag\w*|desactiva|corta\w*|cort\w*|cero|off|seguro|deja\w*)\b.*\b(salidas?|luces?|reles?|todo\s+off|off)\b/.test(
      norm
    )
  ) {
    return accept(
      norm,
      "SENSOR_FAULT_FAILSAFE",
      baseRevision,
      [
        {
          op: "upsertRule",
          ruleId: "temp.high-alarm",
          templateId: "thresholdHoldWithLatchedHysteresis",
          input: "temp.process",
          unit: "degC_x10",
          triggerAtOrAbove: 450,
          clearAtOrBelow: 440,
          holdMs: ctx.activeHoldMs ?? 10_000,
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: "tower.red",
          timingStateOnChange: "PRESERVE_ELAPSED",
          stableBlockId: 1001,
          blockType: "TON",
        },
      ],
      "Ante falla o degradacion del sensor de temperatura, comandar salidas OFF y reiniciar temporizador."
    );
  }

  // Intent 6: Remove prealarm / rule ("Limpia o quita la regla de prealarma")
  if (
    /\b(limpia|quita|elimina|borra|remueve|saca)\b/.test(norm) &&
    /\b(prealarma|pre alarma|banda|azul)\b/.test(norm)
  ) {
    return accept(
      norm,
      "REMOVE_PREALARM_RULE",
      baseRevision,
      [
        {
          op: "removeRule",
          ruleId: "temp.prealarm-pulse",
          templateId: "bandPrealarmPulse",
          input: "temp.process",
          unit: "degC_x10",
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: "tower.blue",
          timingStateOnChange: "RESET",
          stableBlockId: 1002,
          blockType: "PULSE_GEN",
        },
      ],
      "Eliminar la regla de prealarma (tower.blue)."
    );
  }

  // Intent 7: Force OFF / disable virtual blue light ("Mantén la luz azul apagada aunque pase el umbral")
  if (
    /\b(manten|mantener|deja|dejar|fuerza|forzar|bloquea)\b/.test(norm) &&
    /\b(azul|blue)\b/.test(norm) &&
    /\b(apagada|apagado|off|desactivada)\b/.test(norm)
  ) {
    return accept(
      norm,
      "KEEP_BLUE_OFF",
      baseRevision,
      [
        {
          op: "upsertRule",
          ruleId: "temp.prealarm-pulse",
          templateId: "manualMask",
          input: "temp.process",
          unit: "degC_x10",
          triggerAtOrAbove: 1250,
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: "tower.blue",
          timingStateOnChange: "RESET",
          stableBlockId: 1002,
          blockType: "PULSE_GEN",
        },
      ],
      "Mantener el canal virtual tower.blue apagado aunque se supere el umbral."
    );
  }

  // Intent 4: Band prealarm pulse ("Si la temperatura esta entre 40 C y 45 C haz parpadear la luz azul cada 2 segundos")
  const bandMatch = norm.match(
    /\bentre\s+(\d+(?:\.\d+)?)\s*(?:c|celsius)?\s+y\s+(\d+(?:\.\d+)?)\s*(?:c|celsius)?\b/
  );
  if (bandMatch && /\b(parpade\w*|puls\w*|destell\w*|titil\w*)\b/.test(norm)) {
    const lowC = Number(bandMatch[1]);
    const highC = Number(bandMatch[2]);
    if (lowC >= highC) {
      return reject(norm, "Rechazado: el limite inferior debe ser menor que el limite superior.");
    }
    const periodMs = parseDurationMs(norm) ?? 2000;
    const ch = detectChannel(norm);
    if (ch.ambiguous || !ch.channel) {
      return clarify(norm, "¿Que luz debe parpadear en esa banda (azul virtual, roja o verde)?");
    }
    return accept(
      norm,
      "BAND_PREALARM_PULSE",
      baseRevision,
      [
        {
          op: "upsertRule",
          ruleId: ch.channel === "tower.blue" ? "temp.prealarm-pulse" : `${ch.channel}.band-pulse`,
          templateId: "bandPrealarmPulse",
          input: "temp.process",
          unit: "degC_x10",
          bandLow: Math.round(lowC * 10),
          bandHigh: Math.round(highC * 10),
          pulsePeriodMs: periodMs,
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: ch.channel,
          timingStateOnChange: "RESET",
          ...(ch.channel === "tower.blue" ? { stableBlockId: 1002 } : {}),
          blockType: "PULSE_GEN",
        },
      ],
      `Parpadear ${ch.channel} cada ${(periodMs / 1000).toFixed(1)} s entre ${lowC} C y ${highC} C.`
    );
  }

  // Intent 2: Change hold timer ("Cambia el retardo de 10 segundos a 5 segundos")
  const retardoFromTo = norm.match(
    /\b(?:cambia\w*|modifica\w*|ajusta\w*|reduce\w*|baja\w*|sube\w*|aumenta\w*|pon\w*|pasa\w*)\b.*\b(?:retardo|tiempo|espera|timer|temporizador|hold)\b.*\bde\s+(\d+(?:\.\d+)?)\s*(?:segundos?|seg|s|milisegundos?|ms|minutos?|min)\s+a\s+(\d+(?:\.\d+)?)\s*(segundos?|seg|s|milisegundos?|ms|minutos?|min)\b/
  );
  if (retardoFromTo) {
    const u = retardoFromTo[3]!;
    const newMs = u.startsWith("ms") || u.startsWith("mili")
      ? Math.round(Number(retardoFromTo[2]))
      : u.startsWith("min")
        ? Math.round(Number(retardoFromTo[2]) * 60_000)
        : Math.round(Number(retardoFromTo[2]) * 1000);
    const ch = detectChannel(norm);
    const targetOut = ch.channel ?? "tower.red";
    if (ch.ambiguous) {
      return clarify(norm, "¿Para que luz desea cambiar el retardo?");
    }
    return accept(
      norm,
      "UPDATE_HOLD_TIMER",
      baseRevision,
      [
        {
          op: "upsertRule",
          ruleId: "temp.high-alarm",
          templateId: "thresholdHoldWithLatchedHysteresis",
          input: "temp.process",
          unit: "degC_x10",
          triggerAtOrAbove: 450,
          clearAtOrBelow: 440,
          holdMs: newMs,
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: targetOut,
          timingStateOnChange: "PRESERVE_ELAPSED",
          stableBlockId: 1001,
          blockType: "TON",
        },
      ],
      `Actualizar retardo de ${targetOut} a ${(newMs / 1000).toFixed(2)} s preservando tiempo transcurrido.`
    );
  }

  // Direct retardo change when explicitly mentioning "alarma alta" / "luz roja" / "roja" with new duration
  if (
    /\b(cambia|ajusta|reduce|sube|pon)\b.*\b(retardo|temporizador|tiempo)\b/.test(norm) &&
    parseDurationMs(norm) !== undefined
  ) {
    const ch = detectChannel(norm);
    const newMs = parseDurationMs(norm)!;
    if (!ch.channel) {
      return clarify(
        norm,
        "¿A que regla o canal (ej. luz roja / alarma alta) desea aplicar ese retardo, y cual era el valor anterior?"
      );
    }
    return accept(
      norm,
      "UPDATE_HOLD_TIMER",
      baseRevision,
      [
        {
          op: "upsertRule",
          ruleId: ch.channel === "tower.red" ? "temp.high-alarm" : `${ch.channel}.hold`,
          templateId: "thresholdHoldWithLatchedHysteresis",
          input: "temp.process",
          unit: "degC_x10",
          triggerAtOrAbove: 450,
          clearAtOrBelow: 440,
          holdMs: newMs,
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: ch.channel,
          timingStateOnChange: "PRESERVE_ELAPSED",
          ...(ch.channel === "tower.red" ? { stableBlockId: 1001 } : {}),
          blockType: "TON",
        },
      ],
      `Actualizar retardo de ${ch.channel} a ${(newMs / 1000).toFixed(2)} s.`
    );
  }

  // Intent 3: Turn on green light while process permit is active ("Enciende la luz verde mientras el proceso este encendido")
  if (
    /\b(enciend\w*|prend\w*|activ\w*|manten\w*|pon\w*)\b/.test(norm) &&
    /\b(verde|green)\b/.test(norm) &&
    /\b(mientras|cuando|si)\b.*\b(proceso|marcha|arranque|permit|encendido|activo)\b/.test(norm)
  ) {
    const hasStartStop = ctx.profile?.sensors.some((s) => s.name === "start.stop") ?? false;
    return accept(
      norm,
      "COPY_PROCESS_PERMIT",
      baseRevision,
      [
        hasStartStop
          ? {
              op: "upsertRule",
              ruleId: "process.run-green",
              templateId: "copyBoolean",
              input: "start.stop",
              unit: "bool",
              requiredQuality: "GOOD",
              onInputFault: "COMMAND_OFF_AND_RESET",
              output: "tower.green",
              timingStateOnChange: "RESET",
              stableBlockId: 1003,
              blockType: "THRESHOLD_HYST",
            }
          : {
              op: "upsertRule",
              ruleId: "process.run-green",
              templateId: "manualMask",
              input: "temp.process",
              unit: "degC_x10",
              triggerAtOrAbove: -400,
              requiredQuality: "GOOD",
              onInputFault: "COMMAND_OFF_AND_RESET",
              output: "tower.green",
              timingStateOnChange: "RESET",
              stableBlockId: 1003,
              blockType: "THRESHOLD_HYST",
            },
      ],
      hasStartStop
        ? "Encender la luz verde (tower.green) mientras la senal start.stop este activa."
        : "Encender la luz verde (tower.green) mientras temp.process este saludable (GOOD) y el proceso activo."
    );
  }

  // Intent 1: Temperature threshold + hold + optional hysteresis
  const chInfo = detectChannel(norm);
  const triggerM = norm.match(
    /\b(?:pasa\s+de|supera\w*|mayor\s+(?:a|o\s+igual\s+a)|llega\s+a|sube\s+de|arriba\s+de|sobre|encima\s+de)\s+(-?\d+(?:\.\d+)?)\s*(c|celsius)?\b/
  );
  const clearM = norm.match(
    /\b(?:baje\s+de|baja\s+de|menor\s+(?:a|o\s+igual\s+a)|debajo\s+de|caiga\s+de|desciende\s+de)\s+(-?\d+(?:\.\d+)?)\s*(c|celsius)?\b/
  );
  if (triggerM && clearM && Number(clearM[1]) > Number(triggerM[1])) {
    return reject(
      norm,
      "Rechazado: el umbral de apagado (histeresis) no puede ser mayor que el umbral de encendido."
    );
  }
  if (/\b(enciend\w*|prend\w*|activ\w*|pon\w*|coloc\w*)\b/.test(norm) && triggerM) {
    if (chInfo.ambiguous) {
      return clarify(norm, "Indico multiples colores; especifique un solo canal de salida.");
    }
    if (!chInfo.channel) {
      return clarify(norm, "¿Que luz desea encender (roja, verde o azul virtual)?");
    }
    const trigVal = Number(triggerM[1]);
    const clearVal = clearM ? Number(clearM[1]) : trigVal;
    const holdMs = parseDurationMs(norm);
    const hasTempContext = /\btemperatura|sensor|temp\b/.test(norm) || triggerM[2] !== undefined;
    if (!hasTempContext) {
      return clarify(norm, "¿El umbral numerico corresponde a temperatura en grados Celsius?");
    }
    return accept(
      norm,
      "TEMP_THRESHOLD_ALARM",
      baseRevision,
      [
        {
          op: "upsertRule",
          ruleId: chInfo.channel === "tower.red" ? "temp.high-alarm" : `${chInfo.channel}.temp-alarm`,
          templateId: "thresholdHoldWithLatchedHysteresis",
          input: "temp.process",
          unit: "degC_x10",
          triggerAtOrAbove: Math.round(trigVal * 10),
          clearAtOrBelow: Math.round(clearVal * 10),
          holdMs: holdMs ?? 0,
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: chInfo.channel,
          timingStateOnChange: "RESET",
          ...(chInfo.channel === "tower.red" ? { stableBlockId: 1001 } : {}),
          blockType: "TON",
        },
      ],
      `Encender ${chInfo.channel} si temp.process >= ${trigVal} C${holdMs ? ` por ${(holdMs / 1000).toFixed(1)} s` : ""} y apagar <= ${clearVal} C.`
    );
  }

  // Turn off explicitly? Clarify if missing target condition, else manualMask / clarify
  if (/\b(para|apaga|apagar|deten|desactiva)\b/.test(norm)) {
    if (chInfo.ambiguous) {
      return clarify(norm, "Especifique un unico canal para apagar (roja, verde o azul virtual).");
    }
    if (!chInfo.channel) {
      return clarify(norm, "¿Que salida o luz desea apagar?");
    }
    if (!/\bsi\b|\bcuando\b|\bmientras\b/.test(norm)) {
      return clarify(
        norm,
        `¿Desea crear una regla permanente que mantenga ${chInfo.channel} apagada, o ejecutar un comando manual temporal (TTL)?`
      );
    }
  }

  if (/\b(enciende|prende|activa)\b/.test(norm) && !chInfo.channel) {
    return clarify(norm, "¿Que luz desea encender y bajo que condicion?");
  }

  if (/\b(cambia|modifica|ajusta|mueve|pon)\b/.test(norm)) {
    return clarify(
      norm,
      "Orden incompleta: indique parametro exacto, valor anterior/nuevo y canal o regla objetivo."
    );
  }

  return clarify(
    norm,
    "No se pudo mapear la instruccion a una regla determinista. Especifique condicion, umbral en C, retardo en s y canal."
  );
}
