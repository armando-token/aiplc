import Foundation

#if canImport(Speech)
import Speech
#endif

public protocol SpeechCaptureService {
    var localeIdentifier: String { get }
    var supportsOnDeviceRecognition: Bool { get }
    var requiresOnDeviceRecognition: Bool { get }
    func startPushToTalk(onPartial: @escaping (String) -> Void, onError: @escaping (String) -> Void)
    func stopPushToTalk() -> String
}

/// Apple Speech framework adapter targeting offline `es-PE` push-to-talk dictation on iPadOS.
public final class AppleSpeechRecognizer: SpeechCaptureService {
    public let localeIdentifier = "es-PE"
    public var requiresOnDeviceRecognition = true
    private var latestTranscript = ""

    public init() {}

    public var supportsOnDeviceRecognition: Bool {
        #if canImport(Speech)
        return SFSpeechRecognizer(locale: Locale(identifier: localeIdentifier))?.supportsOnDeviceRecognition ?? false
        #else
        return false
        #endif
    }

    public func startPushToTalk(onPartial: @escaping (String) -> Void, onError: @escaping (String) -> Void) {
        #if canImport(Speech)
        guard let recognizer = SFSpeechRecognizer(locale: Locale(identifier: localeIdentifier)) else {
            onError("Reconocedor es-PE no disponible")
            return
        }
        if requiresOnDeviceRecognition && !recognizer.supportsOnDeviceRecognition {
            onError("Reconocimiento offline no soportado; habilite entrada manual o descargue paquete es-PE.")
            return
        }
        latestTranscript = ""
        onPartial("")
        #else
        onError("Speech framework unavailable on this platform")
        #endif
    }

    public func stopPushToTalk() -> String {
        latestTranscript
    }
}

/// Deterministic mock for offline verification without microphone hardware (V01 / V02).
public final class MockSpeechCaptureService: SpeechCaptureService {
    public let localeIdentifier = "es-PE"
    public let supportsOnDeviceRecognition: Bool
    public let requiresOnDeviceRecognition = true
    private let scriptedTranscript: String

    public init(scriptedTranscript: String, supportsOnDeviceRecognition: Bool = true) {
        self.scriptedTranscript = scriptedTranscript
        self.supportsOnDeviceRecognition = supportsOnDeviceRecognition
    }

    public func startPushToTalk(onPartial: @escaping (String) -> Void, onError: @escaping (String) -> Void) {
        if !supportsOnDeviceRecognition {
            onError("Dictado offline no disponible; use teclado local.")
            return
        }
        onPartial(scriptedTranscript)
    }

    public func stopPushToTalk() -> String {
        scriptedTranscript
    }
}
