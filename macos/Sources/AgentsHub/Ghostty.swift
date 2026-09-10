import GhosttyTerminal
import SwiftUI

/// The one shared `TerminalController`, and the only file besides `TerminalSession` that
/// touches libghostty. That is deliberate: libghostty's embedding API is officially
/// unstable, so keeping the surface down to a handful of symbols in two files makes an
/// upgrade a two-file diff by construction.
///
/// Shared, not one per session: each `TerminalController.init` creates a `ghostty_app_t`,
/// and the package's own comment says every surface shares one.
@MainActor
enum Ghostty {
    /// Ghostty's *own* config file, so a terminal in this app is the same terminal the
    /// user already configured — font, theme, colours, keybinds, opacity, all of it.
    /// Anything set here instead would silently override it, so nothing is set here.
    ///
    /// The macOS app support path wins because that is where Ghostty itself writes; the
    /// XDG path is the documented alternative. Note the `.ghostty` extension — the
    /// plain `config` name is the older spelling and both are still in the wild.
    static let configPath: String? = {
        let fm = FileManager.default
        let env = ProcessInfo.processInfo.environment
        let home = NSHomeDirectory()
        let appSupport = "\(home)/Library/Application Support/com.mitchellh.ghostty"
        let xdg = env["XDG_CONFIG_HOME"].flatMap { $0.isEmpty ? nil : $0 } ?? "\(home)/.config"

        let candidates = [
            env["GHOSTTY_CONFIG"],
            "\(appSupport)/config.ghostty",
            "\(appSupport)/config",
            "\(xdg)/ghostty/config",
        ].compactMap { $0 }

        return candidates.first { fm.fileExists(atPath: $0) }
    }()

    static let controller: TerminalController = {
        guard let path = configPath else {
            // No Ghostty install to borrow from: a readable default rather than
            // whatever the bare library falls back to.
            return TerminalController { b in
                b.withFontFamily("SF Mono")
                b.withFontSize(13)
            }
        }
        return TerminalController(configFilePath: path)
    }()

    static func apply(_ scheme: ColorScheme) {
        // The package's ColorScheme initialiser is internal, so map it here.
        controller.setColorScheme(scheme == .dark ? .dark : .light)
    }
}

/// System / light / dark, because a terminal that follows a light system appearance
/// looks wrong next to the dark terminal everyone actually configures.
enum AppAppearance: String, CaseIterable, Identifiable {
    case system, light, dark

    var id: String { rawValue }

    var label: String {
        switch self {
        case .system: return "Match System"
        case .light: return "Light"
        case .dark: return "Dark"
        }
    }

    var colorScheme: ColorScheme? {
        switch self {
        case .system: return nil
        case .light: return .light
        case .dark: return .dark
        }
    }
}
