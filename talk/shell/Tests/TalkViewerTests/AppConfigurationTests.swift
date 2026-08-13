import Foundation
import XCTest

@testable import TalkViewer

final class AppConfigurationTests: XCTestCase {
    func testDefaultsToTheBlitDevelopmentUI() {
        let configuration = AppConfiguration.current(arguments: [], environment: [:])

        XCTAssertEqual(configuration.initialURL.absoluteString, "http://127.0.0.1:10000")
        XCTAssertNil(configuration.preferredDevice)
    }

    func testLocalhostGetsHTTPWithoutTreatingThePortAsAURLScheme() {
        let url = AppConfiguration.normalizedURL(from: "localhost:3000/demo")

        XCTAssertEqual(url?.absoluteString, "http://localhost:3000/demo")
    }

    func testCommandLineOptionsOverrideEnvironment() {
        let configuration = AppConfiguration.current(
            arguments: ["--url", "https://slides.example", "--device", "Stage iPad"],
            environment: [
                "TALK_VIEWER_URL": "https://old.example",
                "TALK_VIEWER_DEVICE": "Old iPad",
            ]
        )

        XCTAssertEqual(configuration.initialURL.absoluteString, "https://slides.example")
        XCTAssertEqual(configuration.preferredDevice, "Stage iPad")
    }
}
