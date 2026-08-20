import AppKit

final class FocusSinkDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow!
    private var statePath: String?
    private var inputEvents = 0

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = NSWindow(
            contentRect: NSRect(x: 80, y: 80, width: 320, height: 160),
            styleMask: [.titled],
            backing: .buffered,
            defer: false
        )
        window.title = "CP-07 Focus Sink"
        window.isReleasedWhenClosed = false
        window.isRestorable = false
        window.sharingType = .readOnly
        let label = NSTextField(labelWithString: "Background focus sentinel")
        label.frame = NSRect(x: 30, y: 60, width: 260, height: 30)
        window.contentView?.addSubview(label)
        window.makeKeyAndOrderFront(nil)
        window.orderFrontRegardless()
        NSApp.activate(ignoringOtherApps: true)

        if CommandLine.arguments.count > 1 {
            statePath = CommandLine.arguments[1]
            writeState()
        }
        NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp, .leftMouseDown, .leftMouseUp, .scrollWheel]) { [weak self] event in
            self?.inputEvents += 1
            self?.writeState()
            return event
        }
    }

    private func writeState() {
        guard let statePath else { return }
        let state = "{\"pid\":\(ProcessInfo.processInfo.processIdentifier),\"window_id\":\(window.windowNumber),\"input_events\":\(inputEvents)}"
        try? state.write(toFile: statePath, atomically: true, encoding: .utf8)
    }
}

let app = NSApplication.shared
let delegate = FocusSinkDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
