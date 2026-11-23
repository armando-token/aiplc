import Foundation

public enum SpanishParseOutcome: String, Equatable, Sendable {
    case accept = "ACCEPT"
    case clarify = "CLARIFY"
    case reject = "REJECT"
}

public struct SpanishIntentPreview: Equatable, Sendable {
    public let outcome: SpanishParseOutcome
    public let normalizedText: String
    public let intentKind: String
    public let targetChannel: String?
    public let virtualWarning: Bool
    public let summaryEs: String
    public let requiresConfirmation: Bool
}

/// On-device Spanish (`es-PE`) preflight intent normalizer & ambiguity blocker for iPad (V01 / V02).
public struct SpanishIntentClient {
    public init() {}

    public func normalize(_ raw: String) -> String {
        var s = raw.lowercased()
            .folding(options: .diacriticInsensitive, locale: Locale(identifier: "es_PE"))
            .replacingOccurrences(of: "°c", with: " c ")
            .replacingOccurrences(of: "°", with: " ")
            .replacingOccurrences(of: "celcius", with: "celsius")
            .replacingOccurrences(of: #"\bgrados?\s+centigrados?\b"#, with: "c", options: .regularExpression)
            .replacingOccurrences(of: #"\bgrados?\s+celsius\b"#, with: "c", options: .regularExpression)
            .replacingOccurrences(of: #"\bgrados?(?!\s*(?:fahrenheit|kelvin|f\b|k\b))\b"#, with: "c", options: .regularExpression)
            .replacingOccurrences(of: "\\s+", with: " ", options: .regularExpression)
            .trimmingCharacters(in: .whitespacesAndNewlines)

        let tens: [String: Int] = ["veinte": 20, "treinta": 30, "cuarenta": 40, "cincuenta": 50, "sesenta": 60, "setenta": 70, "ochenta": 80, "noventa": 90]
        let units: [String: Int] = ["cero": 0, "uno": 1, "una": 1, "dos": 2, "tres": 3, "cuatro": 4, "cinco": 5, "seis": 6, "siete": 7, "ocho": 8, "nueve": 9, "diez": 10, "once": 11, "doce": 12, "quince": 15, "veinte": 20]
        let tokens = s.split(separator: " ").map(String.init)
        var out: [String] = []
        var idx = 0
        while idx < tokens.count {
            let t = tokens[idx]
            if let tv = tens[t], idx + 2 < tokens.count, tokens[idx + 1] == "y", let uv = units[tokens[idx + 2]], uv < 10 {
                out.append(String(tv + uv))
                idx += 3
            } else if let uv = units[t] {
                out.append(String(uv))
                idx += 1
            } else {
                out.append(t)
                idx += 1
            }
        }
        s = out.joined(separator: " ")
        return s
    }

    public func preflightParse(_ raw: String) -> SpanishIntentPreview {
        let norm = normalize(raw)
        if norm.isEmpty {
            return SpanishIntentPreview(
                outcome: .clarify,
                normalizedText: norm,
                intentKind: "EMPTY",
                targetChannel: nil,
                virtualWarning: false,
                summaryEs: "Indique instruccion valida en espanol.",
                requiresConfirmation: false
            )
        }

        // Strict reject: safety bypass or foreign units or out-of-range temperature
        if norm.contains("ignora la calidad")
            || norm.contains("desactiva los interlocks")
            || norm.contains("quita el inhibidor")
            || norm.contains("aunque el sensor este malo")
            || norm.range(of: #"\b\d+(?:\.\d+)?\s*(?:grados?\s+)?(f|fahrenheit|kelvin|psi|bar|volts?|amperios?|rpm)\b"#, options: .regularExpression) != nil
        {
            return SpanishIntentPreview(
                outcome: .reject,
                normalizedText: norm,
                intentKind: "REJECTED",
                targetChannel: nil,
                virtualWarning: false,
                summaryEs: "Rechazado: unidad incompatible o intento de desactivar proteccion.",
                requiresConfirmation: false
            )
        }

        if let regex = try? NSRegularExpression(pattern: #"(-?\d+(?:\.\d+)?)\s*(?:c|celsius)\b"#) {
            let ns = norm as NSString
            let matches = regex.matches(in: norm, range: NSRange(location: 0, length: ns.length))
            for m in matches {
                if let val = Double(ns.substring(with: m.range(at: 1))), (val > 125.0 || val < -40.0) {
                    return SpanishIntentPreview(
                        outcome: .reject,
                        normalizedText: norm,
                        intentKind: "REJECTED",
                        targetChannel: nil,
                        virtualWarning: false,
                        summaryEs: "Rechazado: temperatura fuera del rango valido THT-02 (-40 C a 125 C).",
                        requiresConfirmation: false
                    )
                }
            }
        }

        let hasRed = norm.range(of: #"\b(roja|rojo)\b"#, options: .regularExpression) != nil
        let hasGreen = norm.range(of: #"\bverde\b"#, options: .regularExpression) != nil
        let hasBlue = norm.range(of: #"\bazul\b"#, options: .regularExpression) != nil
        let colorCount = [hasRed, hasGreen, hasBlue].filter { $0 }.count

        if colorCount > 1 {
            return SpanishIntentPreview(
                outcome: .clarify,
                normalizedText: norm,
                intentKind: "AMBIGUOUS",
                targetChannel: nil,
                virtualWarning: false,
                summaryEs: "Especifique un solo canal (roja, verde o azul virtual).",
                requiresConfirmation: false
            )
        }

        if norm.contains("prealarma") && (norm.contains("quita") || norm.contains("limpia")) {
            return SpanishIntentPreview(
                outcome: .accept,
                normalizedText: norm,
                intentKind: "REMOVE_PREALARM_RULE",
                targetChannel: "tower.blue",
                virtualWarning: true,
                summaryEs: "Eliminar regla de prealarma en tower.blue (VIRTUAL).",
                requiresConfirmation: true
            )
        }

        if norm.contains("retardo") && norm.range(of: #"\bde\s+\d+.*\ba\s+\d+"#, options: .regularExpression) != nil {
            return SpanishIntentPreview(
                outcome: .accept,
                normalizedText: norm,
                intentKind: "UPDATE_HOLD_TIMER",
                targetChannel: "tower.red",
                virtualWarning: false,
                summaryEs: "Actualizar temporizador TON preservando tiempo transcurrido.",
                requiresConfirmation: true
            )
        }

        if hasBlue && norm.contains("parpad") && norm.contains("entre") {
            return SpanishIntentPreview(
                outcome: .accept,
                normalizedText: norm,
                intentKind: "BAND_PREALARM_PULSE",
                targetChannel: "tower.blue",
                virtualWarning: true,
                summaryEs: "Pulso de prealarma en canal VIRTUAL tower.blue.",
                requiresConfirmation: true
            )
        }

        if (hasRed || hasGreen) && norm.range(of: #"\b\d+(?:\.\d+)?\s*(c|celsius)\b"#, options: .regularExpression) != nil {
            return SpanishIntentPreview(
                outcome: .accept,
                normalizedText: norm,
                intentKind: "TEMP_THRESHOLD_ALARM",
                targetChannel: hasRed ? "tower.red" : "tower.green",
                virtualWarning: false,
                summaryEs: "Regla de umbral de temperatura en grados Celsius.",
                requiresConfirmation: true
            )
        }

        return SpanishIntentPreview(
            outcome: .clarify,
            normalizedText: norm,
            intentKind: "AMBIGUOUS",
            targetChannel: nil,
            virtualWarning: false,
            summaryEs: "Instruccion ambigua o incompleta: no se genera cambio automatico.",
            requiresConfirmation: false
        )
    }
}
