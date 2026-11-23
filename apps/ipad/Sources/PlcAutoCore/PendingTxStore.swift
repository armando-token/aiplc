import Foundation

/// Atomic JSON disk store persisting in-flight `txId`s across iPad Wi-Fi drops or restarts (D02 / V02).
public final class PendingTxStore {
    private let fileUrl: URL
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    public init(fileUrl: URL) {
        self.fileUrl = fileUrl
    }

    public func loadAll() throws -> [PendingTxRecord] {
        guard FileManager.default.fileExists(atPath: fileUrl.path) else {
            return []
        }
        let data = try Data(contentsOf: fileUrl)
        return try decoder.decode([PendingTxRecord].self, from: data)
    }

    public func upsert(_ record: PendingTxRecord) throws {
        var all = try loadAll().filter { $0.txId != record.txId }
        all.append(record)
        try writeAtomic(all)
    }

    public func remove(txId: String) throws {
        let filtered = try loadAll().filter { $0.txId != txId }
        try writeAtomic(filtered)
    }

    private func writeAtomic(_ records: [PendingTxRecord]) throws {
        let dir = fileUrl.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let data = try encoder.encode(records)
        try data.write(to: fileUrl, options: .atomic)
    }
}
