import SwiftUI

struct ContentView: View {
    @ObservedObject var browser: BrowserModel
    @ObservedObject var capture: CaptureModel

    @FocusState private var addressIsFocused: Bool

    var body: some View {
        HSplitView {
            browserPane
                .frame(minWidth: 480)

            devicePane
                .frame(minWidth: 360)
        }
        .frame(minWidth: 1000, minHeight: 640)
        .background(Color(nsColor: .windowBackgroundColor))
        .onAppear {
            capture.start()
        }
        .onDisappear {
            capture.stop()
        }
    }

    private var browserPane: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Button(action: browser.goBack) {
                    Image(systemName: "chevron.left")
                }
                .disabled(!browser.canGoBack)
                .help("Back")

                Button(action: browser.goForward) {
                    Image(systemName: "chevron.right")
                }
                .disabled(!browser.canGoForward)
                .help("Forward")

                Button(action: browser.reloadOrStop) {
                    Image(systemName: browser.isLoading ? "xmark" : "arrow.clockwise")
                }
                .help(browser.isLoading ? "Stop" : "Reload")

                TextField("URL", text: $browser.address)
                    .textFieldStyle(.roundedBorder)
                    .focused($addressIsFocused)
                    .onSubmit(browser.loadAddress)

                Button("Go", action: browser.loadAddress)
                    .keyboardShortcut(.return, modifiers: [])
            }
            .buttonStyle(.borderless)
            .controlSize(.large)
            .padding(.horizontal, 12)
            .frame(height: 50)

            ZStack(alignment: .top) {
                WebView(webView: browser.webView)

                if browser.isLoading {
                    ProgressView(value: browser.estimatedProgress)
                        .progressViewStyle(.linear)
                }
            }
        }
    }

    private var devicePane: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Label("iPad display", systemImage: "ipad")
                    .font(.headline)

                Spacer()

                if capture.devices.isEmpty {
                    Text("No device")
                        .foregroundStyle(.secondary)
                } else {
                    Picker(
                        "iPad",
                        selection: Binding(
                            get: { capture.selectedDeviceID },
                            set: capture.selectDevice(id:)
                        )
                    ) {
                        ForEach(capture.devices) { device in
                            Text(device.name).tag(device.id)
                        }
                    }
                    .labelsHidden()
                    .frame(maxWidth: 240)
                }

                Button(action: capture.refreshDevices) {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .help("Refresh devices")

                Divider()
                    .frame(height: 18)

                Button(action: capture.zoomOut) {
                    Image(systemName: "minus.magnifyingglass")
                }
                .buttonStyle(.borderless)
                .disabled(capture.magnification <= 1)
                .help("Zoom out")

                Button(action: capture.resetZoom) {
                    Text(magnificationLabel)
                        .monospacedDigit()
                        .frame(minWidth: 38)
                }
                .buttonStyle(.borderless)
                .help("Fit iPad display")

                Button(action: capture.zoomIn) {
                    Image(systemName: "plus.magnifyingglass")
                }
                .buttonStyle(.borderless)
                .disabled(capture.magnification >= 8)
                .help("Zoom in")
            }
            .padding(.horizontal, 14)
            .frame(height: 50)

            ZStack {
                Color.black
                CapturePreview(
                    session: capture.session,
                    magnification: $capture.magnification
                )

                if !capture.isShowingVideo {
                    VStack(spacing: 14) {
                        Image(systemName: "cable.connector")
                            .font(.system(size: 42, weight: .light))
                            .foregroundStyle(.secondary)
                        Text(capture.status)
                            .multilineTextAlignment(.center)
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: 360)
                    }
                    .padding(30)
                }
            }

            HStack {
                Circle()
                    .fill(capture.isShowingVideo ? Color.green : Color.orange)
                    .frame(width: 8, height: 8)
                Text(capture.status)
                    .font(.caption)
                    .lineLimit(1)
                Spacer()
                Text("Pinch to zoom · Scroll to pan")
                    .font(.caption)
            }
            .foregroundStyle(.secondary)
            .padding(.horizontal, 14)
            .frame(height: 30)
        }
    }

    private var magnificationLabel: String {
        if capture.magnification <= 1.001 {
            return "Fit"
        }
        return "\(Int((capture.magnification * 100).rounded()))%"
    }
}
