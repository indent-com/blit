import Foundation

struct AppConfiguration {
    static let defaultURL = URL(string: "http://127.0.0.1:10000")!

    let initialURL: URL
    let preferredDevice: String?

    static func current(
        arguments: [String] = Array(ProcessInfo.processInfo.arguments.dropFirst()),
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> AppConfiguration {
        var urlValue = environment["TALK_VIEWER_URL"]
        var preferredDevice = environment["TALK_VIEWER_DEVICE"]
        var position = 0

        while position < arguments.count {
            switch arguments[position] {
            case "--url" where position + 1 < arguments.count:
                position += 1
                urlValue = arguments[position]
            case "--device" where position + 1 < arguments.count:
                position += 1
                preferredDevice = arguments[position]
            default:
                if !arguments[position].hasPrefix("-"), urlValue == nil {
                    urlValue = arguments[position]
                }
            }
            position += 1
        }

        return AppConfiguration(
            initialURL: urlValue.flatMap(normalizedURL(from:)) ?? defaultURL,
            preferredDevice: preferredDevice
        )
    }

    static func normalizedURL(from rawValue: String) -> URL? {
        let value = rawValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { return nil }

        let lowercased = value.lowercased()
        let isLocal = lowercased.hasPrefix("localhost")
            || lowercased.hasPrefix("127.")
            || lowercased.hasPrefix("[::1]")
        if isLocal {
            return URL(string: "http://" + value)
        }

        if value.contains("://")
            || lowercased.hasPrefix("about:")
            || lowercased.hasPrefix("data:")
            || lowercased.hasPrefix("file:")
        {
            return URL(string: value)
        }

        return URL(string: "https://" + value)
    }
}
