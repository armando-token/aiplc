#if canImport(SwiftUI)
import SwiftUI
import PlcAutoCore

@available(iOS 17.0, macOS 14.0, *)
public struct PlcConsoleDashboardView: View {
    public let state: DeviceStateDto
    public let connectionBanner: String
    public let channels: [ChannelStatusItem]
    public let pendingIntentPreview: SpanishIntentPreview?
    public let onTapAuthorize: () -> Void
    public let onTapActivate: () -> Void
    public let onTapCancel: () -> Void
    public let onTapRollback: () -> Void
    public let onTapRearm: () -> Void

    public init(
        state: DeviceStateDto,
        connectionBanner: String,
        channels: [ChannelStatusItem],
        pendingIntentPreview: SpanishIntentPreview? = nil,
        onTapAuthorize: @escaping () -> Void = {},
        onTapActivate: @escaping () -> Void = {},
        onTapCancel: @escaping () -> Void = {},
        onTapRollback: @escaping () -> Void = {},
        onTapRearm: @escaping () -> Void = {}
    ) {
        self.state = state
        self.connectionBanner = connectionBanner
        self.channels = channels
        self.pendingIntentPreview = pendingIntentPreview
        self.onTapAuthorize = onTapAuthorize
        self.onTapActivate = onTapActivate
        self.onTapCancel = onTapCancel
        self.onTapRollback = onTapRollback
        self.onTapRearm = onTapRearm
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(connectionBanner)
                .font(.caption)
                .foregroundColor(.secondary)

            HStack {
                Text("PLC \(state.deviceId) · Rev \(state.activeRevision)")
                    .font(.headline)
                Spacer()
                Button("Rearmar", action: onTapRearm)
            }

            if let ev = state.lastEvidence {
                VStack(alignment: .leading, spacing: 4) {
                    Text("SELECTED_IN_SCAN: \(ev.selectedInScan ? "SI" : "NO")")
                    Text("DRIVER_APPLIED: \(ev.isDriverApplied ? "SI (\(ev.driverApplied))" : "NO (\(ev.driverApplied))")")
                    Text("PHYSICAL_OBSERVED: \(ev.isPhysicalObserved ? "SI" : "NO")")
                    Text("ACTIVE_PERSISTED: \(ev.activePersisted ? "SI" : "NO")")
                    if ev.hasWitnessDiscrepancy {
                        Text("ALERTA TESTIGO: applied=\(ev.appliedMask) observed=\(ev.physicalObservedMask.map(String.init) ?? "nil") (\(ev.guardReason))")
                            .foregroundColor(.orange)
                    }
                }
                .font(.caption.monospaced())
            }

            ForEach(channels) { ch in
                HStack {
                    Text(ch.label)
                    Text(ch.isPhysical ? "FISICO" : "VIRTUAL")
                        .font(.caption2.bold())
                    Spacer()
                    Text(ch.isOn ? "ON" : "OFF")
                }
            }

            if let preview = pendingIntentPreview {
                VStack(alignment: .leading, spacing: 6) {
                    Text(preview.summaryEs)
                    HStack {
                        Button("Autorizar", action: onTapAuthorize)
                        Button("Activar", action: onTapActivate)
                        Button("Cancelar", action: onTapCancel)
                        Button("Rollback", action: onTapRollback)
                    }
                }
            }
        }
        .padding()
    }
}
#endif
