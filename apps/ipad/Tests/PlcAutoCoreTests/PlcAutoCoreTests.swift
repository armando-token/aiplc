import Foundation
import XCTest
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
@testable import PlcAutoCore

final class StubTransport: HttpTransport, @unchecked Sendable {
    var handler: (URLRequest) throws -> (Data, HTTPURLResponse)

    init(handler: @escaping (URLRequest) throws -> (Data, HTTPURLResponse)) {
        self.handler = handler
    }

    func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        try handler(request)
    }
}

final class PlcAutoCoreTests: XCTestCase {
    func testChannelStatusExplicitlyMarksBlueVirtualAndRedGreenPhysical() {
        let items = ChannelStatusItem.from(appliedMask: 0b101)
        XCTAssertEqual(items.count, 3)
        XCTAssertTrue(items[0].isPhysical)
        XCTAssertTrue(items[1].isPhysical)
        XCTAssertFalse(items[2].isPhysical)
        XCTAssertTrue(items[2].label.contains("VIRTUAL"))
    }

    func testD02AndV02PendingTxStoreReconcilesSameTxIdAfterReconnect() async throws {
        let tmpDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("plc-ipad-test-\(UUID().uuidString)")
        let storeUrl = tmpDir.appendingPathComponent("pending.json")
        let store = PendingTxStore(fileUrl: storeUrl)
        try store.upsert(PendingTxRecord(
            txId: "tx-wifi-drop-1",
            deviceId: "lyra-bench-01",
            action: .activate,
            expectedPackageHash: "abc123",
            createdAtEpochMs: 1_700_000_000_000
        ))

        var requestedPaths: [String] = []
        let stub = StubTransport { req in
            requestedPaths.append("\(req.httpMethod ?? "") \(req.url?.path ?? "")")
            let body = """
            {
              "activeRevision": "2",
              "transaction": {
                "txId": "tx-wifi-drop-1",
                "revision": {
                  "txId": "tx-wifi-drop-1",
                  "deviceId": "lyra-bench-01",
                  "revision": "2",
                  "packageHash": "abc123",
                  "state": "STABLE"
                }
              }
            }
            """.data(using: .utf8)!
            let http = HTTPURLResponse(url: req.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!
            return (body, http)
        }

        let client = PlcApiClient(
            baseUrl: URL(string: "http://127.0.0.1:8443")!,
            bearerToken: "plc-operator-token",
            transport: stub,
            txStore: store
        )

        let outcomes = await client.reconcilePendingTransactions(deviceId: "lyra-bench-01")
        XCTAssertEqual(requestedPaths, ["GET /v1/proposals/tx-wifi-drop-1"])
        XCTAssertEqual(outcomes, [.settled(txId: "tx-wifi-drop-1", remoteState: "STABLE", activeRevision: "2")])
        let remaining = try store.loadAll()
        XCTAssertTrue(remaining.isEmpty)
        try? FileManager.default.removeItem(at: tmpDir)
    }

    func testSpanishPreflightAndOfflineSpeechMockNeverAutoCommitAmbiguity() {
        let speech = MockSpeechCaptureService(
            scriptedTranscript: "Cambia el retardo de 10 segundos a 5 segundos"
        )
        var captured = ""
        speech.startPushToTalk(onPartial: { captured = $0 }, onError: { XCTFail($0) })
        let intentClient = SpanishIntentClient()

        let ok = intentClient.preflightParse(captured)
        XCTAssertEqual(ok.outcome, .accept)
        XCTAssertTrue(ok.requiresConfirmation)

        let amb = intentClient.preflightParse("Enciende la luz")
        XCTAssertEqual(amb.outcome, .clarify)
        XCTAssertFalse(amb.requiresConfirmation)

        let rej = intentClient.preflightParse("Ignora la calidad del sensor")
        XCTAssertEqual(rej.outcome, .reject)
        XCTAssertFalse(rej.requiresConfirmation)

        let rejFahr = intentClient.preflightParse("Enciende la luz roja si supera 100 grados fahrenheit")
        XCTAssertEqual(rejFahr.outcome, .reject)

        let spoken = intentClient.preflightParse("Si pasa de cuarenta y cinco grados enciende la luz roja")
        XCTAssertEqual(spoken.outcome, .accept)
        XCTAssertEqual(spoken.targetChannel, "tower.red")
    }

    func testDecodesRealRustSupervisorStateWithEvidenceBundle() throws {
        let json = """
        {
          "deviceId": "lyra-bench-01",
          "bootId": "99",
          "scanId": "42",
          "activeRevision": "3",
          "controlArmed": true,
          "fallbackLatched": false,
          "needsOperatorReview": false,
          "missedReleases": "0",
          "lastEvidence": {
            "bootId": "99",
            "scanId": "42",
            "activeRevision": "3",
            "outputSeq": "17",
            "selectedInScan": true,
            "guardReason": "NONE",
            "desiredMask": 3,
            "appliedMask": 3,
            "driverApplied": "APPLIED",
            "physicalObservedMask": 1,
            "physicalObservationSource": "gpio-chardev-v2-readback",
            "activePersisted": true,
            "observedAtNs": "1000000",
            "expiresAtNs": "2000000"
          }
        }
        """.data(using: .utf8)!
        let state = try JSONDecoder().decode(DeviceStateDto.self, from: json)
        XCTAssertEqual(state.activeRevision, "3")
        XCTAssertEqual(state.lastEvidence?.isDriverApplied, true)
        XCTAssertEqual(state.lastEvidence?.isPhysicalObserved, true)
        XCTAssertEqual(state.lastEvidence?.hasWitnessDiscrepancy, true)
    }
}
