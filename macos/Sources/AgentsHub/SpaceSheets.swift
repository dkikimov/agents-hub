import AgentsHubCore
import SwiftUI

/// Names a space, new or existing.
struct SpaceNameSheet: View {
    let title: String
    let action: String
    let initial: String
    let submit: (String) -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var name = ""

    private var blank: Bool { name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(Ghostty.ui(Ghostty.fontSize + 1, weight: .bold))
            TextField("name", text: $name)
                .textFieldStyle(.roundedBorder)
                .onSubmit(done)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(action, action: done).keyboardShortcut(.defaultAction).disabled(blank)
            }
        }
        .padding(16)
        .frame(width: 360)
        .font(Ghostty.ui())
        .onAppear { name = initial }
    }

    private func done() {
        guard !blank else { return }
        submit(name)
        dismiss()
    }
}

struct SpaceDeleteConfirm: View {
    @ObservedObject var model: AppModel
    let id: UUID
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        let count = model.sessions(in: .space(id)).count
        VStack(alignment: .leading, spacing: 14) {
            Text("Delete space \(model.title(of: .space(id)))?")
                .font(Ghostty.ui(Ghostty.fontSize + 1, weight: .bold))
            Text("Its \(count) session\(count == 1 ? "" : "s") move to Unsorted. Nothing is killed.")
                .font(Ghostty.ui(Ghostty.fontSize - 2)).foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Delete", role: .destructive) {
                    model.deleteSpace(id, confirmed: true)
                    dismiss()
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(16)
        .frame(width: 380)
        .font(Ghostty.ui())
    }
}

struct PickItem: Identifiable {
    let id: String
    let title: String
    let detail: String
    let mood: Mood
    let pick: () -> Void
}

/// A filter over a list, run from the keyboard: type to narrow, ↑/↓ to choose, ⏎ to take.
/// Every word typed must appear somewhere in a row. Given `create`, a query no row is
/// named exactly is offered as a new entry at the bottom.
struct PickerSheet: View {
    let title: String
    let items: [PickItem]
    var create: ((String) -> Void)?

    @Environment(\.dismiss) private var dismiss
    @AppStorage("theme") private var theme = Theme.classic
    @State private var query = ""
    @State private var highlighted = 0

    private var trimmed: String { query.trimmingCharacters(in: .whitespacesAndNewlines) }

    private var shown: [PickItem] {
        let words = trimmed.lowercased().split(separator: " ")
        return items.filter { item in
            let hay = (item.title + " " + item.detail).lowercased()
            return words.allSatisfy { hay.contains($0) }
        }
    }

    private var offersCreate: Bool {
        create != nil && !trimmed.isEmpty
            && !items.contains { $0.title.caseInsensitiveCompare(trimmed) == .orderedSame }
    }

    var body: some View {
        let rows = shown
        VStack(alignment: .leading, spacing: 10) {
            Text(title).font(Ghostty.ui(Ghostty.fontSize + 1, weight: .bold))
            TextField(create == nil ? "filter" : "filter, or name a new space", text: $query)
                .textFieldStyle(.roundedBorder)
                .onChange(of: query) { _, _ in highlighted = 0 }
                .onSubmit(take)
                .onKeyPress(.downArrow) { step(1) }
                .onKeyPress(.upArrow) { step(-1) }
            ScrollViewReader { proxy in
                ScrollView {
                    VStack(alignment: .leading, spacing: 0) {
                        ForEach(Array(rows.enumerated()), id: \.element.id) { i, item in
                            row(i) {
                                MoodDot(mood: item.mood)
                                Text(item.title).lineLimit(1)
                                Text(item.detail).foregroundStyle(.secondary)
                                    .lineLimit(1).truncationMode(.middle)
                            }
                            .onTapGesture { take(item) }
                        }
                        if offersCreate {
                            row(rows.count) {
                                Image(systemName: "plus").foregroundStyle(.secondary)
                                Text("New space “\(trimmed)”")
                            }
                            .onTapGesture { make() }
                        }
                    }
                }
                .onChange(of: highlighted) { _, i in proxy.scrollTo(i) }
            }
            .frame(height: 260)
            .background(.quaternary)
            .cornerRadius(4)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
            }
        }
        .padding(16)
        .frame(width: 520)
        .font(Ghostty.ui())
    }

    private func row<Content: View>(_ index: Int, @ViewBuilder _ content: () -> Content) -> some View {
        HStack(spacing: 6) {
            content()
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 6).padding(.vertical, 3)
        .background(index == highlighted ? theme.palette.accent.opacity(0.25) : .clear)
        .contentShape(Rectangle())
        .id(index)
    }

    private func step(_ delta: Int) -> KeyPress.Result {
        let count = shown.count + (offersCreate ? 1 : 0)
        guard count > 0 else { return .ignored }
        highlighted = min(max(highlighted + delta, 0), count - 1)
        return .handled
    }

    private func take() {
        if let item = shown[safe: highlighted] {
            take(item)
        } else if offersCreate {
            make()
        }
    }

    private func take(_ item: PickItem) {
        dismiss()
        item.pick()
    }

    private func make() {
        dismiss()
        create?(trimmed)
    }
}

extension AppModel {
    func moveTargets(_ keys: [SessionKey]) -> [PickItem] {
        book.targets(from: Set(keys.map(spaceID(of:)))).map { space in
            let view = space.map(SpaceView.space) ?? .unsorted
            let count = sessions(in: view).count
            return PickItem(id: "\(view)", title: title(of: view),
                            detail: "\(count) session\(count == 1 ? "" : "s")",
                            mood: mood(of: view)) { [weak self] in self?.move(keys, to: space) }
        }
    }

    /// Spaces first, then every session, most urgent first, so ⌘P⏎ goes to whatever is
    /// waiting on you.
    var goToItems: [PickItem] {
        let spaces = barViews.map { view in
            PickItem(id: "v/\(view)", title: title(of: view), detail: "space",
                     mood: mood(of: view)) { [weak self] in self?.selectSpace(view) }
        }
        let sessions = vms.indices
            .flatMap { vi in
                vms[vi].sessions.filter { !$0.isShell }.map { (SessionKey(vm: vi, id: $0.id), $0) }
            }
            .sorted { mood(of: $0.0) > mood(of: $1.0) }
            .map { key, info in
                let place = [spaceName(of: key) ?? "Unsorted", vms[key.vm].name, info.cwd]
                return PickItem(id: "s/\(key.vm)/\(key.id)", title: "\(info.agent) \(info.name)",
                                detail: place.joined(separator: " · "),
                                mood: mood(of: key)) { [weak self] in self?.reveal(key) }
            }
        return spaces + sessions
    }
}
