import AppKit

/// The menu bar icon: a waveform, with a red "recording" dot while
/// streaming, a hollow ring while looking for the PC, a yellow exclamation
/// badge while streaming to a PC that is not playing (warning), and an
/// orange exclamation badge on error.
///
/// Idle and searching are template images (the system tints them for light
/// and dark menu bars). The streaming and error variants carry colour, so
/// they are not templates; their waveform is drawn in `labelColor` inside a
/// drawing handler, which AppKit runs with the status item's effective
/// appearance, so it is dark on a light menu bar and light on a dark one,
/// while the dot stays red.
enum MenuIcon {
    enum State: Equatable, CaseIterable {
        case idle, searching, streaming, warning, error, code
    }

    static let size = NSSize(width: 20, height: 16)

    /// One image per state, kept for the life of the app: the status item
    /// redraws whenever it is handed a different NSImage object, and the
    /// label is re-evaluated far more often than the state changes.
    @MainActor private static var cache: [State: NSImage] = [:]

    @MainActor static func image(_ state: State) -> NSImage {
        if let hit = cache[state] { return hit }
        let img = draw(state)
        cache[state] = img
        return img
    }

    private static func draw(_ state: State) -> NSImage {
        switch state {
        case .code:
            return symbol("number.circle", template: true)
        case .idle:
            return symbol("waveform", template: true)
        case .searching:
            return badged(template: true) { rect in
                let ring = NSBezierPath(ovalIn: rect.insetBy(dx: 0.9, dy: 0.9))
                ring.lineWidth = 1.6
                NSColor.black.setStroke()
                ring.stroke()
            }
        case .streaming:
            return badged(template: false) { rect in
                NSColor.systemRed.setFill()
                NSBezierPath(ovalIn: rect).fill()
            }
        case .warning:
            return badged(template: false) { rect in
                NSColor.systemYellow.setFill()
                NSBezierPath(ovalIn: rect).fill()
                let bar = NSRect(x: rect.midX - 0.75, y: rect.minY + 3.2, width: 1.5, height: rect.height - 5)
                NSColor.black.setFill()
                NSBezierPath(roundedRect: bar, xRadius: 0.7, yRadius: 0.7).fill()
                NSBezierPath(ovalIn: NSRect(x: rect.midX - 0.8, y: rect.minY + 1.2, width: 1.6, height: 1.6)).fill()
            }
        case .error:
            return badged(template: false) { rect in
                NSColor.systemOrange.setFill()
                NSBezierPath(ovalIn: rect).fill()
                let bar = NSRect(x: rect.midX - 0.75, y: rect.minY + 3.2, width: 1.5, height: rect.height - 5)
                NSColor.white.setFill()
                NSBezierPath(roundedRect: bar, xRadius: 0.7, yRadius: 0.7).fill()
                NSBezierPath(ovalIn: NSRect(x: rect.midX - 0.8, y: rect.minY + 1.2, width: 1.6, height: 1.6)).fill()
            }
        }
    }

    private static func symbol(_ name: String, template: Bool) -> NSImage {
        let cfg = NSImage.SymbolConfiguration(pointSize: 14, weight: .regular)
        let img = NSImage(systemSymbolName: name, accessibilityDescription: "Tightwire")?.withSymbolConfiguration(cfg) ?? NSImage()
        img.isTemplate = template
        return img
    }

    /// Waveform with a badge in the top-right corner, cut out of the waveform
    /// so it reads at menu bar size.
    private static func badged(template: Bool, badge: @escaping (NSRect) -> Void) -> NSImage {
        let img = NSImage(size: size, flipped: false) { bounds in
            let wave = symbol("waveform", template: true)
            // Shrink the waveform a little so the badge sits clear of it.
            let k = min(1, 15 / wave.size.width)
            let w = wave.size.width * k, h = wave.size.height * k
            let waveRect = NSRect(x: 0, y: (bounds.height - h) / 2 - 1, width: w, height: h)
            // Tint the template waveform with the current appearance's label colour.
            let tinted = NSImage(size: wave.size, flipped: false) { r in
                wave.draw(in: r)
                (template ? NSColor.black : NSColor.labelColor).set()
                r.fill(using: .sourceAtop)
                return true
            }
            tinted.draw(in: waveRect)
            let d: CGFloat = 7.5
            let dot = NSRect(x: bounds.maxX - d, y: bounds.maxY - d - 0.5, width: d, height: d)
            // Knock out a margin around the badge.
            NSGraphicsContext.current?.compositingOperation = .clear
            NSBezierPath(ovalIn: dot.insetBy(dx: -1.5, dy: -1.5)).fill()
            NSGraphicsContext.current?.compositingOperation = .sourceOver
            badge(dot)
            return true
        }
        img.isTemplate = template
        img.accessibilityDescription = "Tightwire"
        return img
    }
}
