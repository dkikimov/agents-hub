import AgentsHubCore
import SwiftUI

/// The chips above the sidebar, one per space, each showing the most urgent dot among its
/// sessions, so an agent waiting in a space you are not looking at is still in sight.
/// A chip is also a drop target for session rows.
struct SpaceBar: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let views = model.barViews
        Flow(spacing: 4) {
            ForEach(Array(views.enumerated()), id: \.element) { index, view in
                SpaceChip(model: model, view: view,
                          shortcut: view == .all ? 0 : index < 9 ? index + 1 : nil)
            }
            Button { model.sheet = .newSpace } label: {
                Image(systemName: "plus").padding(.horizontal, 5).padding(.vertical, 3)
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .help("New space (⌃⌘N)")
        }
        .font(Ghostty.ui(Ghostty.fontSize - 2))
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct SpaceChip: View {
    @ObservedObject var model: AppModel
    let view: SpaceView
    let shortcut: Int?

    @AppStorage("theme") private var theme = Theme.classic
    @State private var targeted = false

    var body: some View {
        let active = model.showing == view
        HStack(spacing: 4) {
            MoodDot(mood: model.mood(of: view))
            Text(model.title(of: view)).lineLimit(1).truncationMode(.tail)
            if view == .unsorted {
                Text("\(model.sessions(in: .unsorted).count)").foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 7).padding(.vertical, 2)
        .frame(maxWidth: 150)
        .background(fill(active: active), in: RoundedRectangle(cornerRadius: 4))
        .overlay {
            if targeted {
                RoundedRectangle(cornerRadius: 4).stroke(theme.palette.accent, lineWidth: 1.5)
            }
        }
        .contentShape(RoundedRectangle(cornerRadius: 4))
        .onTapGesture { model.selectSpace(view) }
        .help(shortcut.map { "⌃⌘\($0)" } ?? "")
        .dropDestination(for: String.self) { ids, _ in
            guard view != .all else { return false }
            model.move(rowIDs: ids, to: view)
            return true
        } isTargeted: { targeted = $0 && view != .all }
        .contextMenu {
            if case let .space(id) = view {
                Button("Rename…") { model.sheet = .renameSpace(id) }
                Button("Delete…") { model.deleteSpace(id) }
            }
        }
    }

    private func fill(active: Bool) -> Color {
        active ? theme.palette.accent.opacity(0.35) : Color.secondary.opacity(0.12)
    }
}

/// Left to right, wrapping onto a new line when the next chip would not fit, each at its
/// own width. A grid would square every chip to the longest name.
private struct Flow: Layout {
    var spacing: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let lines = arrange(proposal.width ?? .infinity, subviews)
        return CGSize(width: proposal.width ?? lines.width, height: lines.height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews,
                       cache: inout ()) {
        let lines = arrange(bounds.width, subviews)
        for (view, origin) in zip(subviews, lines.origins) {
            view.place(at: CGPoint(x: bounds.minX + origin.x, y: bounds.minY + origin.y),
                       proposal: .unspecified)
        }
    }

    private func arrange(_ width: CGFloat, _ subviews: Subviews)
        -> (origins: [CGPoint], width: CGFloat, height: CGFloat)
    {
        var origins: [CGPoint] = []
        var x: CGFloat = 0
        var y: CGFloat = 0
        var lineHeight: CGFloat = 0
        var widest: CGFloat = 0
        for view in subviews {
            let size = view.sizeThatFits(.unspecified)
            if x > 0, x + size.width > width {
                x = 0
                y += lineHeight + spacing
                lineHeight = 0
            }
            origins.append(CGPoint(x: x, y: y))
            widest = max(widest, x + size.width)
            x += size.width + spacing
            lineHeight = max(lineHeight, size.height)
        }
        return (origins, widest, y + lineHeight)
    }
}
