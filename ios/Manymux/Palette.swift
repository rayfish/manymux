import SwiftUI
import UIKit

/// The 256 colours a terminal names by number, and the app's own around them.
///
/// The same values as the Android app's `Palette.kt` and `colors.xml`, so the
/// two apps are one product and the chrome matches the session in it. The first
/// sixteen are the ones that vary between terminals; the rest are the cube and
/// the greys, which are defined by arithmetic and are the same everywhere.
enum Palette {
    /// What text and background are when the session asked for neither.
    static let text: UInt32 = 0xD8D8D8
    static let ground: UInt32 = 0x16181A

    private static let named: [UInt32] = [
        0x000000, 0xCC5555, 0x55AA55, 0xCCAA55,
        0x5577CC, 0xAA66CC, 0x55AAAA, 0xD8D8D8,
        0x666666, 0xFF7777, 0x77DD77, 0xFFDD77,
        0x77AAFF, 0xCC99FF, 0x77DDDD, 0xFFFFFF,
    ]

    static func of(_ colour: Colour, or fallback: UInt32) -> UInt32 {
        switch colour {
        case .default:
            return fallback
        case let .rgb(red, green, blue):
            return rgb(UInt32(red), UInt32(green), UInt32(blue))
        case let .indexed(index):
            return indexed(Int(index))
        }
    }

    private static func indexed(_ index: Int) -> UInt32 {
        if index < 16 { return named[index] }
        if index < 232 {
            // The 6x6x6 cube, whose levels are not evenly spaced: the step from
            // nothing to the first level is bigger than the ones after it.
            let n = index - 16
            return rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        let grey = UInt32(8 + (index - 232) * 10)
        return rgb(grey, grey, grey)
    }

    private static func level(_ step: Int) -> UInt32 {
        step == 0 ? 0 : UInt32(55 + step * 40)
    }

    private static func rgb(_ red: UInt32, _ green: UInt32, _ blue: UInt32) -> UInt32 {
        (red << 16) | (green << 8) | blue
    }
}

extension UIColor {
    convenience init(rgb: UInt32, alpha: CGFloat = 1) {
        self.init(
            red: CGFloat((rgb >> 16) & 0xFF) / 255,
            green: CGFloat((rgb >> 8) & 0xFF) / 255,
            blue: CGFloat(rgb & 0xFF) / 255,
            alpha: alpha
        )
    }
}

extension Color {
    init(rgb: UInt32) {
        self.init(uiColor: UIColor(rgb: rgb))
    }

    static let ground = Color(rgb: Palette.ground)
    /// The bar above a session, and anything else that sits on the ground.
    static let panel = Color(rgb: 0x22252A)
    static let raised = Color(rgb: 0x1D2023)
    static let line = Color(rgb: 0x2C3036)
    static let text = Color(rgb: Palette.text)
    static let dim = Color(rgb: 0x8A9198)
    static let hint = Color(rgb: 0x6C7379)
    /// The bright green of the 16. Everything that means "this one" or "go".
    static let accent = Color(rgb: 0x77DD77)
    static let accentSurface = Color(rgb: 0x294032)
    static let warn = Color(rgb: 0xFFDD77)
}
