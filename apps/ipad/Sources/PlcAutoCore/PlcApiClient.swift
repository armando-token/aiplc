import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

public protocol HttpTransport: Sendable {
    func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse)
}

public final class URLSessionHttpTransport: HttpTransport {
    private let session: URLSession

    public init(session: URLSession = .shared) {
        self.session = session
    }

    public func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let (data, resp) = try await session.data(for: request)
        guard let http = resp as? HTTPURLResponse else {
            throw PlcApiError.invalidResponse
        }
        return (data, http)
    }
}

public enum PlcApiError: Error, Equatable {
    case invalidUrl(String)
    case invalidResponse
    case httpError(statusCode: Int, body: String)
    case transportFailure(String)
}

/// Typed iPad client for all 8 §6.3 Supervisor HTTP routes with idempotent `txId` reconnect reconciliation.
public final class PlcApiClient {
    public let baseUrl: URL
    public let bearerToken: String
    private let transport: HttpTransport
    private let txStore: PendingTxStore?

    public init(
        baseUrl: URL,
        bearerToken: String,
        transport: HttpTransport = URLSessionHttpTransport(),
        txStore: PendingTxStore? = nil
    ) {
        self.baseUrl = baseUrl
        self.bearerToken = bearerToken
        self.transport = transport
        self.txStore = txStore
    }

    private func buildRequest(method: String, path: String, jsonData: Data? = nil) throws -> URLRequest {
        guard let url = URL(string: path, relativeTo: baseUrl) else {
            throw PlcApiError.invalidUrl(path)
        }
        var req = URLRequest(url: url)
        req.httpMethod = method
        req.setValue("Bearer \(bearerToken)", forHTTPHeaderField: "Authorization")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = jsonData
        req.timeoutInterval = 5.0
        return req
    }

    private func execute(method: String, path: String, bodyObj: [String: Any]? = nil) async throws -> Data {
        let data = try bodyObj.map { try JSONSerialization.data(withJSONObject: $0) }
        let req = try buildRequest(method: method, path: path, jsonData: data)
        let (respData, http): (Data, HTTPURLResponse)
        do {
            (respData, http) = try await transport.send(req)
        } catch {
            throw PlcApiError.transportFailure(error.localizedDescription)
        }
        guard (200...299).contains(http.statusCode) else {
            let txt = String(data: respData, encoding: .utf8) ?? ""
            throw PlcApiError.httpError(statusCode: http.statusCode, body: txt)
        }
        return respData
    }

    private func encodePathSegment(_ raw: String) -> String {
        var allowed = CharacterSet.urlPathAllowed
        allowed.remove(charactersIn: "/")
        return raw.addingPercentEncoding(withAllowedCharacters: allowed) ?? raw
    }

    // Route 1: GET /v1/devices/{id}/state
    public func getState(deviceId: String) async throws -> DeviceStateDto {
        let data = try await execute(method: "GET", path: "/v1/devices/\(encodePathSegment(deviceId))/state")
        return try JSONDecoder().decode(DeviceStateDto.self, from: data)
    }

    // Route 2: GET /v1/devices/{id}/telemetry
    public func getTelemetry(deviceId: String) async throws -> Data {
        try await execute(method: "GET", path: "/v1/devices/\(encodePathSegment(deviceId))/telemetry")
    }

    // Route 3: POST /v1/devices/{id}/proposals
    public func propose(deviceId: String, txId: String, packageJson: [String: Any]) async throws -> Data {
        try txStore?.upsert(PendingTxRecord(
            txId: txId,
            deviceId: deviceId,
            action: .propose,
            createdAtEpochMs: Int64(Date().timeIntervalSince1970 * 1000)
        ))
        return try await execute(
            method: "POST",
            path: "/v1/devices/\(encodePathSegment(deviceId))/proposals",
            bodyObj: ["txId": txId, "package": packageJson]
        )
    }

    // Route 4: POST /v1/proposals/{tx}/authorizations
    public func authorize(
        deviceId: String,
        txId: String,
        operatorId: String,
        expectedPackageHash: String,
        ttlMs: Int = 60_000
    ) async throws -> Data {
        try txStore?.upsert(PendingTxRecord(
            txId: txId,
            deviceId: deviceId,
            action: .authorize,
            expectedPackageHash: expectedPackageHash,
            createdAtEpochMs: Int64(Date().timeIntervalSince1970 * 1000)
        ))
        return try await execute(
            method: "POST",
            path: "/v1/proposals/\(encodePathSegment(txId))/authorizations",
            bodyObj: [
                "operatorId": operatorId,
                "expectedPackageHash": expectedPackageHash,
                "ttlMs": ttlMs
            ]
        )
    }

    // Route 5: POST /v1/proposals/{tx}/activate
    public func activate(deviceId: String, txId: String) async throws -> Data {
        try txStore?.upsert(PendingTxRecord(
            txId: txId,
            deviceId: deviceId,
            action: .activate,
            createdAtEpochMs: Int64(Date().timeIntervalSince1970 * 1000)
        ))
        return try await execute(
            method: "POST",
            path: "/v1/proposals/\(encodePathSegment(txId))/activate",
            bodyObj: [:]
        )
    }

    // Route 6: GET /v1/proposals/{tx}
    public func getProposal(txId: String) async throws -> ProposalQueryDto {
        let data = try await execute(method: "GET", path: "/v1/proposals/\(encodePathSegment(txId))")
        return try JSONDecoder().decode(ProposalQueryDto.self, from: data)
    }

    // Route 7: POST /v1/proposals/{tx}/cancel
    public func cancel(txId: String) async throws -> Data {
        try await execute(method: "POST", path: "/v1/proposals/\(encodePathSegment(txId))/cancel", bodyObj: [:])
    }

    // Route 8: POST /v1/devices/{id}/rollbacks
    public func rollback(deviceId: String, txId: String, baseRevision: String, reason: String) async throws -> Data {
        try txStore?.upsert(PendingTxRecord(
            txId: txId,
            deviceId: deviceId,
            action: .rollback,
            createdAtEpochMs: Int64(Date().timeIntervalSince1970 * 1000)
        ))
        return try await execute(
            method: "POST",
            path: "/v1/devices/\(encodePathSegment(deviceId))/rollbacks",
            bodyObj: ["txId": txId, "baseRevision": baseRevision, "reason": reason]
        )
    }

    // Manual & Rearm endpoints
    public func manualOverride(deviceId: String, mask: UInt32, validMask: UInt32, ttlMs: Int) async throws -> Data {
        try await execute(
            method: "POST",
            path: "/v1/devices/\(encodePathSegment(deviceId))/manual",
            bodyObj: ["mask": mask, "validMask": validMask, "ttlMs": ttlMs]
        )
    }

    public func rearm(deviceId: String) async throws -> Data {
        try await execute(method: "POST", path: "/v1/devices/\(encodePathSegment(deviceId))/rearm", bodyObj: [:])
    }

    /// Reconciles all persisted pending transactions using their stored `txId` without inventing duplicates (D02 / V02).
    public func reconcilePendingTransactions(deviceId: String) async -> [ReconcileOutcome] {
        guard let records = try? txStore?.loadAll() else {
            return []
        }
        let terminalStates: Set<String> = [
            "STABLE",
            "REJECTED",
            "SUPERSEDED",
            "ROLLED_BACK",
            "RECOVERY_REQUIRED"
        ]
        var results: [ReconcileOutcome] = []
        for rec in records where rec.deviceId == deviceId {
            do {
                let prop = try await getProposal(txId: rec.txId)
                let state = prop.transaction.revision.state
                if terminalStates.contains(state) {
                    try? txStore?.remove(txId: rec.txId)
                    results.append(.settled(txId: rec.txId, remoteState: state, activeRevision: prop.activeRevision))
                } else {
                    results.append(.verifyingState(txId: rec.txId, message: "estado=\(state), revActiva=\(prop.activeRevision)"))
                }
            } catch PlcApiError.httpError(let statusCode, _) where statusCode == 404 {
                if let devState = try? await getState(deviceId: deviceId) {
                    try? txStore?.remove(txId: rec.txId)
                    results.append(.notFoundOnDevice(txId: rec.txId, activeRevision: devState.activeRevision))
                } else {
                    results.append(.verifyingState(txId: rec.txId, message: "verificando estado"))
                }
            } catch {
                results.append(.verifyingState(txId: rec.txId, message: "verificando estado: \(error)"))
            }
        }
        return results
    }
}
