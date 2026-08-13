import AppKit
import AVFoundation
import QuartzCore
import SwiftUI
import WebKit

struct WebView: NSViewRepresentable {
    let webView: WKWebView

    func makeNSView(context: Context) -> WKWebView {
        webView
    }

    func updateNSView(_ nsView: WKWebView, context: Context) {}
}

struct CapturePreview: NSViewRepresentable {
    let session: AVCaptureSession
    @Binding var magnification: Double

    func makeCoordinator() -> Coordinator {
        Coordinator(magnification: $magnification)
    }

    func makeNSView(context: Context) -> CapturePreviewScrollView {
        let scrollView = CapturePreviewScrollView(session: session)
        context.coordinator.observe(scrollView)
        return scrollView
    }

    func updateNSView(_ nsView: CapturePreviewScrollView, context: Context) {
        context.coordinator.magnification = $magnification
        let requestedMagnification = CGFloat(magnification)
        let clamped = min(
            nsView.maxMagnification,
            max(nsView.minMagnification, requestedMagnification)
        )
        if abs(nsView.magnification - clamped) > 0.001 {
            nsView.setMagnification(clamped, centeredAt: nsView.documentVisibleRect.center)
        }
    }

    final class Coordinator: NSObject {
        var magnification: Binding<Double>
        private var observation: NSKeyValueObservation?

        init(magnification: Binding<Double>) {
            self.magnification = magnification
        }

        func observe(_ scrollView: NSScrollView) {
            observation = scrollView.observe(\.magnification, options: [.new]) { [weak self] view, _ in
                guard let self else { return }
                let value = Double(view.magnification)
                if abs(self.magnification.wrappedValue - value) > 0.001 {
                    self.magnification.wrappedValue = value
                }
            }
        }
    }
}

final class CapturePreviewScrollView: NSScrollView {
    private let previewView: CapturePreviewNSView

    init(session: AVCaptureSession) {
        previewView = CapturePreviewNSView(session: session)
        super.init(frame: .zero)

        drawsBackground = true
        backgroundColor = .black
        borderType = .noBorder
        hasHorizontalScroller = true
        hasVerticalScroller = true
        autohidesScrollers = true
        allowsMagnification = true
        minMagnification = 1
        maxMagnification = 8
        documentView = previewView
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func layout() {
        super.layout()
        let viewportSize = contentView.frame.size
        if previewView.frame.size != viewportSize {
            previewView.frame = NSRect(origin: .zero, size: viewportSize)
        }
    }
}

final class CapturePreviewNSView: NSView {
    private let previewLayer: AVCaptureVideoPreviewLayer

    init(session: AVCaptureSession) {
        previewLayer = AVCaptureVideoPreviewLayer(session: session)
        super.init(frame: .zero)

        wantsLayer = true
        layer?.backgroundColor = NSColor.black.cgColor
        previewLayer.videoGravity = .resizeAspect
        layer?.addSublayer(previewLayer)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func layout() {
        super.layout()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        previewLayer.frame = bounds
        CATransaction.commit()
    }
}

private extension NSRect {
    var center: NSPoint {
        NSPoint(x: midX, y: midY)
    }
}
