const SPOKEN_UNITS: Record<string, number> = {
  cero: 0,
  uno: 1,
  una: 1,
  dos: 2,
  tres: 3,
  cuatro: 4,
  cinco: 5,
  seis: 6,
  siete: 7,
  ocho: 8,
  nueve: 9,
  diez: 10,
  once: 11,
  doce: 12,
  trece: 13,
  catorce: 14,
  quince: 15,
  dieciseis: 16,
  diecisiete: 17,
  dieciocho: 18,
  diecinueve: 19,
  veinte: 20,
  veintiuno: 21,
  veintidos: 22,
  veintitres: 23,
  veinticuatro: 24,
  veinticinco: 25,
  veintiseis: 26,
  veintisiete: 27,
  veintiocho: 28,
  veintinueve: 29,
  treinta: 30,
  cuarenta: 40,
  cincuenta: 50,
  sesenta: 60,
  setenta: 70,
  ochenta: 80,
  noventa: 90,
  cien: 100,
};

const SPOKEN_TENS: Record<string, number> = {
  treinta: 30,
  cuarenta: 40,
  cincuenta: 50,
  sesenta: 60,
  setenta: 70,
  ochenta: 80,
  noventa: 90,
};

const SPOKEN_ONES: Record<string, number> = {
  uno: 1,
  dos: 2,
  tres: 3,
  cuatro: 4,
  cinco: 5,
  seis: 6,
  siete: 7,
  ocho: 8,
  nueve: 9,
};

/**
 * Normalizes Spanish / es-PE / LatAm ASR text:
 * - lowercases & removes diacritics
 * - collapses punctuation / degree signs
 * - maps ASR variants ("celcius", "pará", "apagá", "prendé")
 * - resolves Spanish spoken numbers ("cuarenta y cinco" -> "45", "diez" -> "10")
 */
export function normalizeSpanishIntent(raw: string): string {
  let s = raw
    .toLowerCase()
    .normalize("NFD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/°\s*c\b/g, " c ")
    .replace(/°/g, " ")
    .replace(/\bcelcius\b/g, "celsius")
    .replace(/\bgrados?\s+centigrados?\b/g, "c")
    .replace(/\bgrados?\s+celsius\b/g, "c")
    .replace(/\bgrados?(?!\s*(?:fahrenheit|kelvin|f\b|k\b))\b/g, "c")
    .replace(/\b(en\s+c)\b/g, "c")
    .replace(/(\d+)\s*[.,]\s*(\d+)/g, "$1.$2")
    .replace(/[¿?¡!,;:()"]/g, " ")
    .replace(/\s+/g, " ")
    .trim();

  // Replace compound spoken numbers like "cuarenta y cinco" first
  const tokens = s.split(" ");
  const out: string[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (
      t in SPOKEN_TENS &&
      tokens[i + 1] === "y" &&
      tokens[i + 2] !== undefined &&
      tokens[i + 2]! in SPOKEN_ONES
    ) {
      out.push(String(SPOKEN_TENS[t]! + SPOKEN_ONES[tokens[i + 2]!]!));
      i += 2;
      continue;
    }
    if (t in SPOKEN_UNITS) {
      out.push(String(SPOKEN_UNITS[t]!));
      continue;
    }
    out.push(t);
  }
  s = out.join(" ");

  // Handle decimal spoken numbers like "44 punto 5"
  s = s.replace(/\b(\d+)\s+punto\s+(\d+)\b/g, "$1.$2");
  s = s.replace(/\b(\d+)\s+coma\s+(\d+)\b/g, "$1.$2");
  return s.replace(/\s+/g, " ").trim();
}
