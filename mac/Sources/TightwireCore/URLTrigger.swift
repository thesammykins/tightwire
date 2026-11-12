import Foundation

/// tightwire://deck/pc | mac | toggle (Stream Deck "Open URL" buttons).
/// Any app or web page can open a URL, so they act only once the owner has
/// allowed them; the first one asks (SPEC §9, security).
public enum URLTrigger {
    public enum Decision: Equatable { case act(Wire.DeckTarget), ask(Wire.DeckTarget), ignore(String) }

    public static func target(of url: URL) -> Wire.DeckTarget? {
        guard url.scheme == "tightwire", url.host == "deck" else { return nil }
        switch url.lastPathComponent.lowercased() {
        case "pc": return .pc
        case "mac": return .mac
        case "toggle": return .toggle
        default: return nil
        }
    }

    public static func decide(_ url: URL, allowed: Bool, alreadyAsked: Bool) -> Decision {
        guard let t = target(of: url) else { return .ignore("unknown URL \(url.absoluteString)") }
        if allowed { return .act(t) }
        return alreadyAsked ? .ignore("URL triggers are off (Stream Deck pane)") : .ask(t)
    }
}
