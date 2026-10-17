import Foundation
import Observation

/// The modifiers the row of extra keys holds for the next key, and what a key
/// comes to on the wire under them.
///
/// The rules are the Android app's (`TerminalView.kt`), so a phone of either
/// kind spells a chord the same way. Held rather than pressed with something:
/// a soft keyboard has no ctrl, so the next character typed is the chord, and
/// the key stays lit until then because a modifier you cannot see the state of
/// is one you press twice.
///
/// Kept apart from the view so it can be tested without a screen.
@Observable
final class Held {
    var control = false
    var shift = false

    /// Text from the keyboard, the first character under whatever is held.
    func typed(_ text: String) -> Data {
        guard (control || shift), let first = text.first else {
            return Data(text.utf8)
        }
        var bytes = character(first)
        bytes.append(contentsOf: Data(text.dropFirst().utf8))
        return bytes
    }

    /// One character, under whatever is held.
    func character(_ character: Character) -> Data {
        var typed = character
        if shift {
            shift = false
            let plain = Array("`1234567890-=[]\\;',./")
            let shifted = Array("~!@#$%^&*()_+{}|:\"<>?")
            if let index = plain.firstIndex(of: character) {
                typed = shifted[index]
            } else if let upper = character.uppercased().first {
                typed = upper
            }
        }
        if control {
            control = false
            // Ctrl-A is 1 and Ctrl-] is 0x1d: the low five bits of the letter,
            // which is what a terminal has always sent.
            if let scalar = typed.uppercased().unicodeScalars.first {
                return Data([UInt8(scalar.value & 0x1F)])
            }
        }
        return Data(String(typed).utf8)
    }

    var enter: Data { shifted("\r", "\u{1b}[13;2u", "\u{1b}[13;6u") }
    var tab: Data { shifted("\t", "\u{1b}[Z", "\u{1b}[1;6Z") }
    var escape: Data { shifted("\u{1b}", "\u{1b}", "\u{1b}") }
    var backspace: Data { shifted("\u{7f}", "\u{7f}", "\u{7f}") }

    /// A cursor key, or nil for the plain one.
    ///
    /// Nil because the plain key is not this end's to spell: a session in
    /// application cursor mode wants `ESC O A` where one in normal mode wants
    /// `ESC [ A`, and `Attach.cursorKey` is what knows which.
    func cursor(_ direction: Direction) -> Data? {
        guard shift else { return nil }
        let suffix: String
        switch direction {
        case .up: suffix = "A"
        case .down: suffix = "B"
        case .right: suffix = "C"
        case .left: suffix = "D"
        }
        return shifted("", "\u{1b}[1;2\(suffix)", "\u{1b}[1;6\(suffix)")
    }

    /// A key with a shifted spelling of its own. Shift is let go of either
    /// way; ctrl only when it was part of the chord, so ctrl then tab still
    /// leaves ctrl waiting for a letter.
    private func shifted(_ plain: String, _ shifted: String, _ chord: String) -> Data {
        guard shift else { return Data(plain.utf8) }
        let bytes = Data((control ? chord : shifted).utf8)
        shift = false
        control = false
        return bytes
    }
}
