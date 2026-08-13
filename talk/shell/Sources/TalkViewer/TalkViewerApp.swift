import SwiftUI

@main
@MainActor
struct TalkViewerApp: App {
    @StateObject private var browser: BrowserModel
    @StateObject private var capture: CaptureModel

    init() {
        let configuration = AppConfiguration.current()
        _browser = StateObject(wrappedValue: BrowserModel(initialURL: configuration.initialURL))
        _capture = StateObject(
            wrappedValue: CaptureModel(preferredDevice: configuration.preferredDevice)
        )
    }

    var body: some Scene {
        WindowGroup("Talk Viewer") {
            ContentView(browser: browser, capture: capture)
        }
        .windowStyle(.titleBar)
        .windowToolbarStyle(.unifiedCompact)
        .defaultSize(width: 1440, height: 900)
        .commands {
            CommandGroup(after: .sidebar) {
                Button("Reload Page") {
                    browser.reloadOrStop()
                }
                .keyboardShortcut("r", modifiers: .command)

                Button("Refresh iPad Devices") {
                    capture.refreshDevices()
                }
                .keyboardShortcut("r", modifiers: [.command, .shift])

                Divider()

                Button("Zoom iPad In") {
                    capture.zoomIn()
                }
                .keyboardShortcut("+", modifiers: [.command, .option])

                Button("Zoom iPad Out") {
                    capture.zoomOut()
                }
                .keyboardShortcut("-", modifiers: [.command, .option])

                Button("Fit iPad Display") {
                    capture.resetZoom()
                }
                .keyboardShortcut("0", modifiers: [.command, .option])
            }
        }
    }
}
