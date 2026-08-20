import AppKit

final class FocusInterruptButton: NSButton {
    var interrupt: (() -> Void)?

    override func becomeFirstResponder() -> Bool {
        let accepted = super.becomeFirstResponder()
        interrupt?()
        return accepted
    }
}

final class TrackingWindow: NSWindow {
    var onKeyDown: ((NSEvent) -> Void)?

    override func sendEvent(_ event: NSEvent) {
        if event.type == .keyDown {
            onKeyDown?(event)
        }
        super.sendEvent(event)
    }
}

final class FixtureController: NSObject, NSApplicationDelegate, NSTextFieldDelegate {
    private var window: TrackingWindow!
    private var interrupter: NSWindow!
    private var duplicateWindow: NSWindow?
    private var status: NSTextField!
    private var input: NSTextField!
    private var scrollStatus: NSTextField!
    private var busyTimer: Timer?
    private var commandPath: String?
    private var statePath: String?
    private var moved = false
    private var resized = false
    private var runNonce = ""
    private var keyEvents = 0
    private var lastKey = ""

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = TrackingWindow(
            contentRect: NSRect(x: 0, y: 0, width: 720, height: 520),
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "CP-07 Native Fixture"
        window.isReleasedWhenClosed = false
        window.isRestorable = false
        window.sharingType = .readOnly
        window.collectionBehavior = [.fullScreenNone]

        interrupter = NSWindow(
            contentRect: NSRect(x: 80, y: 80, width: 260, height: 120),
            styleMask: [.titled],
            backing: .buffered,
            defer: false
        )
        interrupter.title = "Interruption Sink"
        interrupter.isReleasedWhenClosed = false
        interrupter.isRestorable = false
        interrupter.sharingType = .readOnly

        let heading = NSTextField(labelWithString: "CP-07 Native Fixture")
        heading.font = .boldSystemFont(ofSize: 28)
        heading.frame = NSRect(x: 30, y: 450, width: 500, height: 40)

        let cascade = button("Cascade", "cascade", #selector(cascadeAction), x: 30)
        let race = button("Capture race", "race", #selector(raceAction), x: 170)
        let busy = button("Never quiet", "busy", #selector(busyAction), x: 330)
        let stop = button("Stop", "stop", #selector(stopAction), x: 490)

        let duplicateOne = button("Duplicate", "duplicate-one", #selector(duplicateAction), x: 600)
        duplicateOne.frame.size.width = 90
        let duplicateTwo = NSButton(title: "Duplicate", target: self, action: #selector(duplicateAction))
        duplicateTwo.setAccessibilityIdentifier("duplicate-two")
        duplicateTwo.frame = NSRect(x: 600, y: 345, width: 90, height: 36)

        input = NSTextField(string: "Ready input")
        input.setAccessibilityIdentifier("input")
        input.placeholderString = "Fixture input"
        input.delegate = self
        input.frame = NSRect(x: 30, y: 325, width: 430, height: 42)

        let interruptBefore = FocusInterruptButton(title: "Interrupt focus", target: nil, action: nil)
        interruptBefore.setAccessibilityIdentifier("interrupt-before")
        interruptBefore.interrupt = { [weak self] in
            self?.interrupter.makeKeyAndOrderFront(nil)
        }
        interruptBefore.frame = NSRect(x: 470, y: 325, width: 120, height: 42)

        let interruptAfter = NSButton(
            title: "Interrupt after",
            target: self,
            action: #selector(interruptAfterAction)
        )
        interruptAfter.setAccessibilityIdentifier("interrupt-after")
        interruptAfter.frame = NSRect(x: 490, y: 280, width: 130, height: 36)

        let terminateAfter = NSButton(
            title: "Terminate",
            target: self,
            action: #selector(terminateAfterAction)
        )
        terminateAfter.setAccessibilityIdentifier("terminate-after")
        terminateAfter.frame = NSRect(x: 625, y: 280, width: 75, height: 36)

        status = NSTextField(labelWithString: "Ready")
        status.setAccessibilityIdentifier("status")
        status.font = .systemFont(ofSize: 22)
        status.isBezeled = true
        status.drawsBackground = true
        status.backgroundColor = .white
        status.frame = NSRect(x: 30, y: 235, width: 660, height: 62)

        let scrollView = NSScrollView(frame: NSRect(x: 30, y: 30, width: 660, height: 170))
        scrollView.setAccessibilityIdentifier("scroll")
        scrollView.hasVerticalScroller = true
        scrollView.contentView.postsBoundsChangedNotifications = true
        let document = NSView(frame: NSRect(x: 0, y: 0, width: 630, height: 700))
        for row in 0..<12 {
            let label = NSTextField(labelWithString: "Scrollable row \(row)")
            label.frame = NSRect(x: 20, y: 640 - row * 52, width: 300, height: 32)
            document.addSubview(label)
        }
        scrollStatus = NSTextField(labelWithString: "Scroll 0")
        scrollStatus.setAccessibilityIdentifier("scroll-status")
        scrollStatus.frame = NSRect(x: 350, y: 640, width: 220, height: 32)
        document.addSubview(scrollStatus)
        scrollView.documentView = document
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(scrolled),
            name: NSView.boundsDidChangeNotification,
            object: scrollView.contentView
        )

        let content = window.contentView!
        [heading, cascade, race, busy, stop, duplicateOne, duplicateTwo, input, interruptBefore,
         interruptAfter, terminateAfter, status, scrollView]
            .forEach(content.addSubview)
        window.onKeyDown = { [weak self] event in
            guard let self, !self.runNonce.isEmpty else { return }
            self.keyEvents += 1
            self.lastKey = event.charactersIgnoringModifiers ?? ""
            self.status.stringValue = "Key — \(self.lastKey) — \(self.runNonce)"
        }
        window.center()
        window.makeKeyAndOrderFront(nil)
        window.orderFrontRegardless()
        window.makeFirstResponder(input)
        NSApp.activate(ignoringOtherApps: true)

        commandPath = Bundle.main.object(forInfoDictionaryKey: "CP07CommandPath") as? String
        statePath = Bundle.main.object(forInfoDictionaryKey: "CP07StatePath") as? String
        Timer.scheduledTimer(
            timeInterval: 0.02,
            target: self,
            selector: #selector(pollCommand),
            userInfo: nil,
            repeats: true
        )

        let ready = "{\"pid\":\(ProcessInfo.processInfo.processIdentifier),\"window_id\":\(window.windowNumber)}"
        print(ready)
        if let readyPath = Bundle.main.object(forInfoDictionaryKey: "CP07ReadyPath") as? String {
            try? ready.write(toFile: readyPath, atomically: true, encoding: .utf8)
        }
        fflush(stdout)
    }

    private func button(_ title: String, _ identifier: String, _ action: Selector, x: CGFloat) -> NSButton {
        let button = NSButton(title: title, target: self, action: action)
        button.setAccessibilityIdentifier(identifier)
        button.frame = NSRect(x: x, y: 395, width: 130, height: 42)
        return button
    }

    @objc private func cascadeAction() {
        busyTimer?.invalidate()
        status.stringValue = "Cascade 1"
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(25)) { [weak self] in
            self?.status.stringValue = "Cascade 2"
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(70)) { [weak self] in
            self?.status.stringValue = "Cascade 3 — settled"
        }
    }

    @objc private func raceAction() {
        busyTimer?.invalidate()
        status.stringValue = "Race 1"
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(90)) { [weak self] in
            self?.status.stringValue = "Race 2 — settled"
        }
    }

    @objc private func busyAction() {
        var count = 0
        busyTimer?.invalidate()
        busyTimer = Timer.scheduledTimer(withTimeInterval: 0.02, repeats: true) { [weak self] _ in
            count += 1
            self?.status.stringValue = "Busy \(count)"
        }
    }

    @objc private func stopAction() {
        busyTimer?.invalidate()
        status.stringValue = "Stopped"
    }

    @objc private func duplicateAction() {
        status.stringValue = "Duplicate"
    }

    @objc private func interruptAfterAction() {
        status.stringValue = "Interrupt after input"
        interrupter.makeKeyAndOrderFront(nil)
    }

    @objc private func terminateAfterAction() {
        NSApp.terminate(nil)
    }

    @objc private func scrolled(_ notification: Notification) {
        guard let clip = notification.object as? NSClipView else { return }
        scrollStatus.stringValue = "Scroll \(Int(clip.bounds.origin.y))"
    }

    @objc private func pollCommand() {
        guard let path = commandPath,
              let command = try? String(contentsOfFile: path, encoding: .utf8)
                .trimmingCharacters(in: .whitespacesAndNewlines),
              !command.isEmpty else { return }
        switch command {
        case "hide": window.orderOut(nil)
        case "unhide":
            NSRunningApplication.current.unhide()
            window.makeKeyAndOrderFront(nil)
        case "minimize": window.miniaturize(nil)
        case "restore":
            window.deminiaturize(nil)
            window.makeKeyAndOrderFront(nil)
        case "move":
            var frame = window.frame
            frame.origin.x += moved ? -80 : 80
            moved.toggle()
            window.setFrame(frame, display: true, animate: false)
        case "resize":
            var frame = window.frame
            frame.size.width += resized ? -40 : 40
            resized.toggle()
            window.setFrame(frame, display: true, animate: false)
        case "interrupt": interrupter.makeKeyAndOrderFront(nil)
        case "occlude":
            interrupter.setFrame(
                window.frame.insetBy(dx: -20, dy: -20),
                display: true,
                animate: false
            )
            interrupter.makeKeyAndOrderFront(nil)
        case "unocclude":
            interrupter.orderOut(nil)
            window.makeKeyAndOrderFront(nil)
        case "duplicate-window":
            let duplicate = NSWindow(
                contentRect: .zero,
                styleMask: [.titled, .closable, .miniaturizable],
                backing: .buffered,
                defer: false
            )
            duplicate.title = window.title
            duplicate.isReleasedWhenClosed = false
            duplicate.isRestorable = false
            duplicate.sharingType = .readOnly
            duplicate.setFrame(window.frame, display: true)
            duplicate.orderFrontRegardless()
            duplicateWindow = duplicate
        case "remove-duplicate-window":
            duplicateWindow?.orderOut(nil)
            duplicateWindow?.close()
            duplicateWindow = nil
        case "snapshot": writeSnapshot()
        default:
            if command.hasPrefix("reset:") {
                reset(String(command.dropFirst("reset:".count)))
            }
        }
        try? FileManager.default.removeItem(atPath: path)
    }

    private func reset(_ nonce: String) {
        runNonce = nonce
        keyEvents = 0
        lastKey = ""
        busyTimer?.invalidate()
        duplicateWindow?.orderOut(nil)
        duplicateWindow?.close()
        duplicateWindow = nil
        interrupter.orderOut(nil)
        status.stringValue = "Ready — \(nonce)"
        input.stringValue = "Input — \(nonce)"
        scrollStatus.stringValue = "Scroll 0"
        if let scrollView = scrollStatus.enclosingScrollView {
            scrollView.contentView.scroll(to: .zero)
            scrollView.reflectScrolledClipView(scrollView.contentView)
        }
        window.title = "CP-09 Native Fixture — \(nonce)"
        window.deminiaturize(nil)
        NSRunningApplication.current.unhide()
        window.makeKeyAndOrderFront(nil)
        window.orderFrontRegardless()
        window.makeFirstResponder(input)
        NSApp.activate(ignoringOtherApps: true)
    }

    private func writeSnapshot() {
        let cgInfo = (CGWindowListCopyWindowInfo(
            [.optionIncludingWindow, .excludeDesktopElements],
            CGWindowID(window.windowNumber)
        ) as? [[String: Any]])?.first
        let cgBounds = cgInfo?[kCGWindowBounds as String] as? [String: Any]
        let snapshot: [String: Any] = [
            "fixture_pid": ProcessInfo.processInfo.processIdentifier,
            "nonce": runNonce,
            "window_title": window.title,
            "key_events": keyEvents,
            "last_key": lastKey,
            "frontmost_pid": NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1,
            "target_window_hash": NSNumber(value: CFHash(window)),
            "target_is_key": window.isKeyWindow,
            "target_is_main": window.isMainWindow,
            "target_is_minimized": window.isMiniaturized,
            "target_is_visible": window.isVisible,
            "status": status.stringValue,
            "input": input.stringValue,
            "pasteboard_change_count": NSPasteboard.general.changeCount,
            "cg_layer": cgInfo?[kCGWindowLayer as String] ?? NSNull(),
            "cg_sharing_state": cgInfo?[kCGWindowSharingState as String] ?? NSNull(),
            "cg_is_onscreen": cgInfo?[kCGWindowIsOnscreen as String] ?? NSNull(),
            "cg_bounds": cgBounds ?? [:],
            "appkit_frame": [
                "x": window.frame.origin.x,
                "y": window.frame.origin.y,
                "width": window.frame.width,
                "height": window.frame.height,
            ],
        ]
        guard let statePath,
              let data = try? JSONSerialization.data(withJSONObject: snapshot, options: [.sortedKeys]) else {
            return
        }
        try? data.write(to: URL(fileURLWithPath: statePath), options: .atomic)
    }

    func controlTextDidChange(_ notification: Notification) {
        status.stringValue = "Input — \(input.stringValue)"
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            status.stringValue = "Enter — \(input.stringValue)"
            return true
        }
        return false
    }
}

let app = NSApplication.shared
let delegate = FixtureController()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
