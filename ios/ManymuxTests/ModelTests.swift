import XCTest

@testable import Manymux

final class ModelTests: XCTestCase {
    func test_failed_initialization_completes_requests_with_its_error() {
        let reason = "the device key could not be read"
        let model = AppModel(openPhone: {
            throw NSError(domain: "test", code: 1, userInfo: [NSLocalizedDescriptionKey: reason])
        })
        var worked = false
        var completed = false
        model.ask({ _ in worked = true }) { answer in
            completed = true
            switch answer {
            case .success:
                XCTFail("a missing client cannot complete a request")
            case let .failure(error):
                XCTAssertEqual(error.localizedDescription, reason)
            }
        }
        XCTAssertFalse(worked)
        XCTAssertTrue(completed, "an async caller must not be left waiting for its continuation")
    }
}
