import XCTest

/// The app against a real machine: add it, see what is running, attach, type,
/// scroll back, and hop to the session next door.
///
/// Needs a machine, so it is skipped unless one is named. Values reach the
/// test runner through `TEST_RUNNER_`-prefixed variables on `xcodebuild`:
///
///     TEST_RUNNER_MM_ADDRESS=build-box TEST_RUNNER_MM_PORT=22 \
///     TEST_RUNNER_MM_USER=somebody TEST_RUNNER_MM_SHOTS=/path/to/shots \
///     xcodebuild test -only-testing:ManymuxUITests ...
///
/// The machine has to have this simulator's key already, a session called
/// `build` with enough history to scroll through, and one called `top` to hop to.
final class ReachTests: XCTestCase {
    private var shots: URL?
    private var taken = 0

    func test_a_machine_is_reached_attached_to_and_typed_into() throws {
        let env = ProcessInfo.processInfo.environment
        guard let address = env["MM_ADDRESS"], let user = env["MM_USER"] else {
            throw XCTSkip("no machine named")
        }
        shots = env["MM_SHOTS"].map { URL(fileURLWithPath: $0) }

        let app = XCUIApplication()
        app.launch()

        app.navigationBars.buttons["Add host"].tap()
        fill(app.textFields["address"], with: address)
        // The form offers the last host's user, so whatever is there goes.
        fill(app.textFields["user"], with: user)
        fill(app.textFields["port"], with: env["MM_PORT"] ?? "22")
        shot("add-host")
        app.buttons["Connect now"].tap()

        let build = app.buttons.containing(.staticText, identifier: "build").firstMatch
        XCTAssertTrue(build.waitForExistence(timeout: 20), "the wall never showed `build`")
        sleep(3)
        shot("wall")

        app.navigationBars.buttons["show them as a list"].tap()
        sleep(1)
        shot("list")
        app.navigationBars.buttons["show them as screens"].tap()

        app.buttons.containing(.staticText, identifier: "build").firstMatch.tap()
        let terminal = app.descendants(matching: .any)["terminal"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 10))
        sleep(3)
        shot("attached")

        app.typeText("echo typed-from-the-phone\n")
        sleep(2)
        shot("typed")

        terminal.swipeDown(velocity: .slow)
        sleep(2)
        shot("scrolled-back")
        terminal.tap()
        sleep(1)

        app.buttons["ctrl"].tap()
        app.typeText("l")
        sleep(1)
        shot("ctrl-l")

        app.navigationBars.buttons["the other sessions on this machine"].tap()
        sleep(1)
        shot("switcher")
        app.buttons["top"].tap()
        sleep(3)
        shot("top")
    }

    private func fill(_ field: XCUIElement, with text: String) {
        field.tap()
        // An empty field reports its placeholder as its value, so the
        // length is only an upper bound on what there is to delete.
        let there = (field.value as? String) ?? ""
        field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: there.count))
        field.typeText(text)
    }

    private func shot(_ name: String) {
        guard let shots else { return }
        taken += 1
        let file = shots.appendingPathComponent(String(format: "%02d-%@.png", taken, name))
        try? XCUIScreen.main.screenshot().pngRepresentation.write(to: file)
    }
}
