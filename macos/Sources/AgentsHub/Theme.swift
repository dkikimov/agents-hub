import SwiftUI

/// The chrome's palette, and only the chrome's: what is inside a terminal is ghostty's
/// business and comes from the user's own config, so a theme here stops at the frame.
struct Palette {
    let vm: Color
    let folder: Color
    let agent: Color
    let running: Color
    /// Wants looking at — a session printing output, or a VM that dropped.
    let attention: Color
    /// The focus ring and every "this one is picked" tint.
    let accent: Color
}

enum Theme: String, CaseIterable, Identifiable {
    case classic, nord, gruvbox, catppuccin, tokyoNight

    var id: String { rawValue }

    var label: String {
        switch self {
        case .classic: return "Classic"
        case .nord: return "Nord"
        case .gruvbox: return "Gruvbox"
        case .catppuccin: return "Catppuccin"
        case .tokyoNight: return "Tokyo Night"
        }
    }

    var palette: Palette {
        switch self {
        case .classic:
            // The system colours, which follow light/dark and the user's accent on their own.
            return Palette(vm: .cyan, folder: .blue, agent: .purple,
                           running: .green, attention: .yellow, accent: .accentColor)
        case .nord:
            return Palette(vm: hex(0x88C0D0), folder: hex(0x81A1C1), agent: hex(0xB48EAD),
                           running: hex(0xA3BE8C), attention: hex(0xEBCB8B), accent: hex(0x88C0D0))
        case .gruvbox:
            return Palette(vm: hex(0x8EC07C), folder: hex(0x83A598), agent: hex(0xD3869B),
                           running: hex(0xB8BB26), attention: hex(0xFABD2F), accent: hex(0xFE8019))
        case .catppuccin:
            return Palette(vm: hex(0x94E2D5), folder: hex(0x89B4FA), agent: hex(0xCBA6F7),
                           running: hex(0xA6E3A1), attention: hex(0xF9E2AF), accent: hex(0xF5C2E7))
        case .tokyoNight:
            return Palette(vm: hex(0x7DCFFF), folder: hex(0x7AA2F7), agent: hex(0xBB9AF7),
                           running: hex(0x9ECE6A), attention: hex(0xE0AF68), accent: hex(0x7AA2F7))
        }
    }
}

private func hex(_ rgb: UInt32) -> Color {
    Color(red: Double((rgb >> 16) & 0xFF) / 255,
          green: Double((rgb >> 8) & 0xFF) / 255,
          blue: Double(rgb & 0xFF) / 255)
}
