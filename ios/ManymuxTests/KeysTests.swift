import XCTest

@testable import Manymux

/// The spelling of a key under the held modifiers, which has to match the
/// Android app's or one phone types something different from the other.
final class KeysTests: XCTestCase {
    func test_a_held_ctrl_turns_the_next_letter_into_its_chord() {
        let held = Held()
        held.control = true
        XCTAssertEqual(held.typed("c"), Data([0x03]))
        XCTAssertFalse(held.control, "a chord lets go of ctrl once it is typed")
        XCTAssertEqual(held.typed("c"), Data("c".utf8))
    }

    func test_ctrl_and_a_bracket_is_the_desktop_mode_key() {
        let held = Held()
        held.control = true
        XCTAssertEqual(held.typed("]"), Data([0x1D]))
    }

    func test_only_the_first_character_of_composed_text_is_chorded() {
        let held = Held()
        held.control = true
        XCTAssertEqual(held.typed("ab"), Data([0x01]) + Data("b".utf8))
    }

    func test_a_held_shift_reaches_the_symbol_above_a_digit() {
        let held = Held()
        held.shift = true
        XCTAssertEqual(held.typed("2"), Data("@".utf8))
        XCTAssertFalse(held.shift)
        held.shift = true
        XCTAssertEqual(held.typed("q"), Data("Q".utf8))
    }

    func test_shift_tab_goes_backwards_and_lets_go_of_both() {
        let held = Held()
        held.shift = true
        XCTAssertEqual(held.tab, Data("\u{1b}[Z".utf8))
        held.shift = true
        held.control = true
        XCTAssertEqual(held.tab, Data("\u{1b}[1;6Z".utf8))
        XCTAssertFalse(held.shift)
        XCTAssertFalse(held.control)
    }

    func test_a_plain_tab_leaves_a_held_ctrl_waiting_for_its_letter() {
        let held = Held()
        held.control = true
        XCTAssertEqual(held.tab, Data("\t".utf8))
        XCTAssertTrue(held.control)
    }

    func test_a_plain_cursor_key_is_left_to_the_session_to_spell() {
        let held = Held()
        XCTAssertNil(held.cursor(.up))
        held.shift = true
        XCTAssertEqual(held.cursor(.left), Data("\u{1b}[1;2D".utf8))
    }

    func test_return_is_a_carriage_return_and_shift_return_is_its_kitty_spelling() {
        let held = Held()
        XCTAssertEqual(held.enter, Data("\r".utf8))
        held.shift = true
        XCTAssertEqual(held.enter, Data("\u{1b}[13;2u".utf8))
    }
}
