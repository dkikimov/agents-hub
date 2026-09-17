import GhosttyTerminal
import GhosttyTheme
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

    /// Why the user's config was ignored, if it was. `nil` is the normal case. Reading it
    /// builds the controller, which is what decides the answer.
    static var configIssue: String? { controller.lastConfigurationIssue }

    /// The user's config as written, and — when ghostty rejects that — the same config
    /// with the one line it cannot satisfy here taken out.
    ///
    /// Ghostty resolves `theme = …` against `GHOSTTY_RESOURCES_DIR`, which the terminal
    /// package pins to its own bundle: shell-integration and terminfo, no themes. A theme
    /// it cannot find is a config diagnostic, *one* diagnostic makes it throw the whole
    /// file away, and the user silently loses their font, their keybinds and
    /// `copy-on-select` over a colour scheme. Pointing the env elsewhere does not help —
    /// ghostty reads it once, before any of this runs — but the package ships the same
    /// theme catalog as Swift data, so the colours survive the detour.
    static let controller: TerminalController = {
        guard let path = configPath else {
            // No Ghostty install to borrow from: a readable default rather than
            // whatever the bare library falls back to.
            return TerminalController { b in
                b.withFontFamily("SF Mono")
                b.withFontSize(13)
            }
        }
        let controller = TerminalController(configFilePath: path)
        if controller.lastConfigurationIssue != nil,
           let text = try? String(contentsOfFile: path, encoding: .utf8) {
            if let theme = configValue("theme").flatMap(catalogTheme) {
                controller.setTheme(theme)
            }
            controller.updateConfigSource(.generated(withoutTheme(text)))
        }
        if let issue = controller.lastConfigurationIssue {
            NSLog("agents-hub: ghostty config ignored, using defaults: %@", issue)
        }
        return controller
    }()

    /// `theme = Dracula`, or ghostty's `theme = dark:Dracula,light:Alabaster`.
    private static func catalogTheme(_ value: String) -> TerminalTheme? {
        var light: TerminalConfiguration?
        var dark: TerminalConfiguration?
        for part in value.split(separator: ",") {
            let bits = part.split(separator: ":", maxSplits: 1)
            let scheme = bits.count == 2 ? bits[0].trimmingCharacters(in: .whitespaces) : ""
            let name = bits[bits.count - 1].trimmingCharacters(in: .whitespaces)
            guard let config = GhosttyThemeCatalog.theme(named: name)?.toTerminalConfiguration()
            else { continue }
            if scheme != "light" { dark = config }
            if scheme != "dark" { light = config }
        }
        guard let fallback = light ?? dark else { return nil }
        return TerminalTheme(light: light ?? fallback, dark: dark ?? fallback)
    }

    private static func withoutTheme(_ text: String) -> String {
        text.split(separator: "\n", omittingEmptySubsequences: false)
            .filter { setting($0)?.key != "theme" }
            .joined(separator: "\n")
    }

    static func apply(_ scheme: ColorScheme) {
        // The package's ColorScheme initialiser is internal, so map it here.
        controller.setColorScheme(scheme == .dark ? .dark : .light)
    }

    // MARK: - borrowing the terminal's font for the rest of the UI

    /// First value for `key` in the ghostty config. Ghostty's format is one `key = value`
    /// per line with `#` comments; repeated keys are fallbacks, so first wins.
    ///
    /// ponytail: does not follow `config-file` includes or resolve a theme's own font.
    /// `ghostty +show-config` would, at the cost of shelling out to an install that may
    /// not be there — do that only if someone's font actually goes missing.
    private static func configValue(_ key: String) -> String? {
        guard let path = configPath,
              let text = try? String(contentsOfFile: path, encoding: .utf8)
        else { return nil }

        for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
            guard let line = setting(raw), line.key == key, !line.value.isEmpty else { continue }
            return line.value
        }
        return nil
    }

    /// One config line as ghostty reads it, or `nil` for a comment or a blank.
    private static func setting(_ raw: Substring) -> (key: String, value: String)? {
        let line = raw.trimmingCharacters(in: .whitespaces)
        guard !line.hasPrefix("#"), let eq = line.firstIndex(of: "=") else { return nil }
        var value = line[line.index(after: eq)...].trimmingCharacters(in: .whitespaces)
        if value.count >= 2, value.hasPrefix("\""), value.hasSuffix("\"") {
            value = String(value.dropFirst().dropLast())
        }
        return (line[..<eq].trimmingCharacters(in: .whitespaces), value)
    }

    /// Only if AppKit can actually resolve it — a family ghostty falls back on would
    /// otherwise leave every label silently rendering in Helvetica.
    static let fontFamily: String? = {
        guard let name = configValue("font-family"), NSFont(name: name, size: 13) != nil else {
            return nil
        }
        return name
    }()

    /// Ghostty's own default when the config does not say.
    static let fontSize: CGFloat = {
        guard let raw = configValue("font-size"), let points = Double(raw) else { return 13 }
        return CGFloat(points)
    }()

    /// The terminal's font, for the chrome around it. Falls back to the system
    /// monospace face, which is the same shape even when the family is missing.
    static func ui(_ size: CGFloat = fontSize, weight: Font.Weight = .regular) -> Font {
        guard let fontFamily else {
            return .system(size: size, weight: weight, design: .monospaced)
        }
        return .custom(fontFamily, fixedSize: size).weight(weight)
    }

    /// The terminal's own background, so the padding around the surface reads as part of
    /// the terminal instead of a border around it. `nil` leaves the window's own
    /// material showing, which is the right answer when there is no config to match.
    static let background: Color? = {
        guard let raw = configValue("background"), let color = Color(ghosttyHex: raw) else {
            return nil
        }
        // Ghostty draws its surface at this opacity; an opaque pad beside it would
        // show as a visible rectangle over a wallpaper.
        let opacity = configValue("background-opacity").flatMap(Double.init) ?? 1
        return color.opacity(max(0, min(1, opacity)))
    }()
}

extension Color {
    /// `#1e1e1e`, `1e1e1e` or the three-digit short form. Named colours are left to the
    /// caller's fallback — ghostty knows hundreds and this only needs the common case.
    init?(ghosttyHex raw: String) {
        var hex = raw.trimmingCharacters(in: .whitespaces)
        if hex.hasPrefix("#") { hex.removeFirst() }
        if hex.count == 3 { hex = hex.map { "\($0)\($0)" }.joined() }
        guard hex.count == 6, let value = UInt32(hex, radix: 16) else { return nil }
        self.init(
            red: Double((value >> 16) & 0xFF) / 255,
            green: Double((value >> 8) & 0xFF) / 255,
            blue: Double(value & 0xFF) / 255
        )
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
