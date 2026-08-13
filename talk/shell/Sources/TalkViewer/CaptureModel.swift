import AVFoundation
import Combine
import CoreGraphics
import CoreMediaIO
import Foundation

struct CaptureDeviceChoice: Identifiable, Hashable {
    let id: String
    let name: String
}

final class CaptureModel: ObservableObject {
    @Published private(set) var devices: [CaptureDeviceChoice] = []
    @Published private(set) var selectedDeviceID = ""
    @Published private(set) var status = "Looking for a USB iPad…"
    @Published private(set) var isShowingVideo = false
    @Published var magnification = 1.0

    let session = AVCaptureSession()

    private let preferredDevice: String?
    private let captureQueue = DispatchQueue(label: "sh.blit.talk-viewer.capture")
    private let discoverySession: AVCaptureDevice.DiscoverySession
    private var currentInput: AVCaptureDeviceInput?
    private var observerTokens: [NSObjectProtocol] = []
    private var started = false
    private var requestedScreenCaptureAccess = false

    init(preferredDevice: String?) {
        self.preferredDevice = preferredDevice

        // Wired iPhone and iPad displays are Core Media I/O screen-capture
        // devices. Opt in before AVFoundation creates its discovery session or
        // the device can remain hidden for the lifetime of this process.
        Self.enableWiredScreenCaptureDevices()
        discoverySession = AVCaptureDevice.DiscoverySession(
            deviceTypes: [.externalUnknown],
            mediaType: .muxed,
            position: .unspecified
        )
    }

    deinit {
        for token in observerTokens {
            NotificationCenter.default.removeObserver(token)
        }
        if session.isRunning {
            session.stopRunning()
        }
    }

    func start() {
        guard !started else {
            if selectedDeviceID.isEmpty {
                requestPermissionAndDiscover()
            } else {
                selectDevice(id: selectedDeviceID)
            }
            return
        }
        started = true

        let center = NotificationCenter.default
        observerTokens = [
            center.addObserver(
                forName: AVCaptureDevice.wasConnectedNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                self?.refreshDevices()
            },
            center.addObserver(
                forName: AVCaptureDevice.wasDisconnectedNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                self?.refreshDevices()
            },
            center.addObserver(
                forName: AVCaptureSession.runtimeErrorNotification,
                object: session,
                queue: .main
            ) { [weak self] notification in
                let error = notification.userInfo?[AVCaptureSessionErrorKey] as? Error
                self?.isShowingVideo = false
                self?.status = error?.localizedDescription ?? "The iPad video session stopped."
            },
        ]

        requestPermissionAndDiscover()

        // Core Media I/O publishes a newly enabled iOS display
        // asynchronously. Connection notifications normally cover this, and
        // these retries close the race where the first notification arrives
        // while the app is still installing its observers.
        for delay in [1.0, 3.0, 8.0] {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
                guard let self, self.devices.isEmpty else { return }
                self.refreshDevices()
            }
        }
    }

    func stop() {
        isShowingVideo = false
        captureQueue.async { [session] in
            if session.isRunning {
                session.stopRunning()
            }
        }
    }

    func refreshDevices() {
        let captureDevices = discoveredDevices()
        let choices = captureDevices.map {
            CaptureDeviceChoice(
                id: $0.uniqueID,
                name: $0.localizedName
            )
        }

        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.devices = choices

            if captureDevices.contains(where: { $0.uniqueID == self.selectedDeviceID }) {
                return
            }

            guard let device = self.preferredDevice(in: captureDevices) else {
                self.selectedDeviceID = ""
                self.isShowingVideo = false
                self.status = "Connect, unlock, and trust an iPad over USB."
                self.removeCurrentInput()
                return
            }

            self.selectDevice(id: device.uniqueID)
        }
    }

    func selectDevice(id: String) {
        guard let device = discoveredDevices().first(where: { $0.uniqueID == id }) else {
            refreshDevices()
            return
        }

        selectedDeviceID = id
        isShowingVideo = false
        magnification = 1
        status = "Connecting to \(device.localizedName)…"
        configureSession(for: device)
    }

    func zoomIn() {
        magnification = min(8, ((magnification + 0.25) * 4).rounded() / 4)
    }

    func zoomOut() {
        magnification = max(1, ((magnification - 0.25) * 4).rounded() / 4)
    }

    func resetZoom() {
        magnification = 1
    }

    private func requestPermissionAndDiscover() {
        guard CGPreflightScreenCaptureAccess() else {
            if !requestedScreenCaptureAccess {
                requestedScreenCaptureAccess = true
                _ = CGRequestScreenCaptureAccess()
            }
            status = "Allow screen recording access, then quit and reopen Talk Viewer."
            return
        }

        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            refreshDevices()
        case .notDetermined:
            status = "Waiting for camera permission…"
            AVCaptureDevice.requestAccess(for: .video) { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self else { return }
                    if granted {
                        self.refreshDevices()
                    } else {
                        self.status = "Allow camera access in System Settings → Privacy & Security."
                    }
                }
            }
        case .denied, .restricted:
            status = "Allow camera access in System Settings → Privacy & Security."
        @unknown default:
            status = "Camera permission is unavailable."
        }
    }

    private static func enableWiredScreenCaptureDevices() {
        var address = CMIOObjectPropertyAddress(
            mSelector: CMIOObjectPropertySelector(kCMIOHardwarePropertyAllowScreenCaptureDevices),
            mScope: CMIOObjectPropertyScope(kCMIOObjectPropertyScopeGlobal),
            mElement: CMIOObjectPropertyElement(kCMIOObjectPropertyElementMain)
        )
        var allow: UInt32 = 1
        _ = CMIOObjectSetPropertyData(
            CMIOObjectID(kCMIOObjectSystemObject),
            &address,
            0,
            nil,
            UInt32(MemoryLayout<UInt32>.size),
            &allow
        )
    }

    private func discoveredDevices() -> [AVCaptureDevice] {
        var seen = Set<String>()

        // `devices(for:)` remains the compatibility path for USB iOS display
        // feeds on older macOS releases. The discovery session handles current
        // systems and connection notifications keep this list fresh.
        let allDevices = discoverySession.devices + AVCaptureDevice.devices(for: .muxed)
        return allDevices
            .filter { seen.insert($0.uniqueID).inserted }
            .sorted { left, right in
                let leftIsIPad = left.localizedName.localizedCaseInsensitiveContains("ipad")
                let rightIsIPad = right.localizedName.localizedCaseInsensitiveContains("ipad")
                if leftIsIPad != rightIsIPad {
                    return leftIsIPad
                }
                return left.localizedName.localizedStandardCompare(right.localizedName) == .orderedAscending
            }
    }

    private func preferredDevice(in devices: [AVCaptureDevice]) -> AVCaptureDevice? {
        if let preferredDevice {
            let match = devices.first {
                $0.uniqueID == preferredDevice
                    || $0.localizedName.localizedCaseInsensitiveContains(preferredDevice)
            }
            if let match { return match }
        }

        return devices.first {
            $0.localizedName.localizedCaseInsensitiveContains("ipad")
        } ?? devices.first
    }

    private func configureSession(for device: AVCaptureDevice) {
        let selectedID = device.uniqueID
        captureQueue.async { [weak self] in
            guard let self else { return }

            do {
                let newInput = try AVCaptureDeviceInput(device: device)

                self.session.beginConfiguration()
                if let currentInput = self.currentInput {
                    self.session.removeInput(currentInput)
                }
                guard self.session.canAddInput(newInput) else {
                    if let currentInput = self.currentInput, self.session.canAddInput(currentInput) {
                        self.session.addInput(currentInput)
                    }
                    self.session.commitConfiguration()
                    throw CaptureError.cannotAddInput
                }

                self.session.addInput(newInput)
                self.preferLargestFormat(on: device)
                self.currentInput = newInput
                self.session.commitConfiguration()
                self.session.startRunning()
                let sourceResolution = self.sourceResolution(of: device)

                DispatchQueue.main.async {
                    guard self.selectedDeviceID == selectedID else { return }
                    self.isShowingVideo = true
                    self.status = sourceResolution.isEmpty
                        ? device.localizedName
                        : "\(device.localizedName) · \(sourceResolution) source"
                }
            } catch {
                DispatchQueue.main.async {
                    guard self.selectedDeviceID == selectedID else { return }
                    self.isShowingVideo = false
                    self.status = "Could not open \(device.localizedName): \(error.localizedDescription)"
                }
            }
        }
    }

    private func removeCurrentInput() {
        captureQueue.async { [weak self] in
            guard let self else { return }
            self.session.beginConfiguration()
            if let currentInput = self.currentInput {
                self.session.removeInput(currentInput)
                self.currentInput = nil
            }
            self.session.commitConfiguration()
            if self.session.isRunning {
                self.session.stopRunning()
            }
        }
    }

    private func preferLargestFormat(on device: AVCaptureDevice) {
        guard let largestFormat = device.formats.max(by: { left, right in
            let leftSize = CMVideoFormatDescriptionGetDimensions(left.formatDescription)
            let rightSize = CMVideoFormatDescriptionGetDimensions(right.formatDescription)
            return Int64(leftSize.width) * Int64(leftSize.height)
                < Int64(rightSize.width) * Int64(rightSize.height)
        }) else { return }

        do {
            try device.lockForConfiguration()
            defer { device.unlockForConfiguration() }
            device.activeFormat = largestFormat
        } catch {
            // Some iPadOS/macOS combinations expose a fixed display format.
            // The capture session can still use that device-selected format.
        }
    }

    private func sourceResolution(of device: AVCaptureDevice) -> String {
        let size = CMVideoFormatDescriptionGetDimensions(device.activeFormat.formatDescription)
        guard size.width > 0, size.height > 0 else { return "" }
        return "\(size.width)×\(size.height)"
    }
}

private enum CaptureError: LocalizedError {
    case cannotAddInput

    var errorDescription: String? {
        "The device does not expose a compatible display stream."
    }
}
