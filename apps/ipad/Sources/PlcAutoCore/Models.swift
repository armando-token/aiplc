import Foundation

public struct EvidenceBundleDto: Codable, Equatable, Sendable {
    public let bootId: String
    public let scanId: String
    public let activeRevision: String
    public let outputSeq: String
    public let selectedInScan: Bool
    public let guardReason: String
    public let desiredMask: UInt32
    public let appliedMask: UInt32
    public let driverApplied: String
    public let physicalObservedMask: UInt32?
    public let physicalObservationSource: String
    public let activePersisted: Bool
    public let observedAtNs: String
    public let expiresAtNs: String

    public var isDriverApplied: Bool {
        driverApplied == "APPLIED"
    }

    public var isPhysicalObserved: Bool {
        physicalObservedMask != nil
    }

    public var hasWitnessDiscrepancy: Bool {
        guard let obs = physicalObservedMask else { return false }
        return obs != (appliedMask & 3)
    }

    public init(
        bootId: String = "1",
        scanId: String = "1",
        activeRevision: String = "1",
        outputSeq: String = "1",
        selectedInScan: Bool,
        guardReason: String = "NONE",
        desiredMask: UInt32,
        appliedMask: UInt32,
        driverApplied: String,
        physicalObservedMask: UInt32?,
        physicalObservationSource: String = "gpio-chardev-v2-readback",
        activePersisted: Bool,
        observedAtNs: String = "0",
        expiresAtNs: String = "0"
    ) {
        self.bootId = bootId
        self.scanId = scanId
        self.activeRevision = activeRevision
        self.outputSeq = outputSeq
        self.selectedInScan = selectedInScan
        self.guardReason = guardReason
        self.desiredMask = desiredMask
        self.appliedMask = appliedMask
        self.driverApplied = driverApplied
        self.physicalObservedMask = physicalObservedMask
        self.physicalObservationSource = physicalObservationSource
        self.activePersisted = activePersisted
        self.observedAtNs = observedAtNs
        self.expiresAtNs = expiresAtNs
    }
}

public struct DeviceStateDto: Codable, Equatable, Sendable {
    public let deviceId: String
    public let bootId: String
    public let scanId: String
    public let activeRevision: String
    public let controlArmed: Bool
    public let fallbackLatched: Bool
    public let needsOperatorReview: Bool
    public let missedReleases: String
    public let lastEvidence: EvidenceBundleDto?
}

public struct DeploymentRevisionDto: Codable, Equatable, Sendable {
    public let txId: String
    public let deviceId: String
    public let revision: String
    public let packageHash: String
    public let state: String
}

public struct ProposalQueryDto: Codable, Equatable, Sendable {
    public let activeRevision: String
    public let lastEvidence: EvidenceBundleDto?

    public struct TxBody: Codable, Equatable, Sendable {
        public let txId: String
        public let revision: DeploymentRevisionDto
    }

    public let transaction: TxBody
}

public struct ChannelStatusItem: Identifiable, Equatable, Sendable {
    public let id: String
    public let label: String
    public let isPhysical: Bool
    public let gpioDescription: String
    public let isOn: Bool

    public static func from(appliedMask: UInt32) -> [ChannelStatusItem] {
        [
            ChannelStatusItem(
                id: "tower.red",
                label: "Luz Roja",
                isPhysical: true,
                gpioDescription: "gpiochip0:17",
                isOn: (appliedMask & 1) != 0
            ),
            ChannelStatusItem(
                id: "tower.green",
                label: "Luz Verde",
                isPhysical: true,
                gpioDescription: "gpiochip0:18",
                isOn: (appliedMask & 2) != 0
            ),
            ChannelStatusItem(
                id: "tower.blue",
                label: "Luz Azul (VIRTUAL)",
                isPhysical: false,
                gpioDescription: "Sin GPIO fisico cableado",
                isOn: (appliedMask & 4) != 0
            )
        ]
    }
}

public enum PendingTxAction: String, Codable, Sendable {
    case propose
    case authorize
    case activate
    case rollback
}

public struct PendingTxRecord: Codable, Equatable, Sendable {
    public let txId: String
    public let deviceId: String
    public let action: PendingTxAction
    public let expectedPackageHash: String?
    public let createdAtEpochMs: Int64

    public init(
        txId: String,
        deviceId: String,
        action: PendingTxAction,
        expectedPackageHash: String? = nil,
        createdAtEpochMs: Int64
    ) {
        self.txId = txId
        self.deviceId = deviceId
        self.action = action
        self.expectedPackageHash = expectedPackageHash
        self.createdAtEpochMs = createdAtEpochMs
    }
}

public enum ReconcileOutcome: Equatable, Sendable {
    case verifyingState(txId: String, message: String)
    case settled(txId: String, remoteState: String, activeRevision: String)
    case notFoundOnDevice(txId: String, activeRevision: String)
}
