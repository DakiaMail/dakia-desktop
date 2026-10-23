import ApplicationServices
import AppKit
import CoreGraphics
import Foundation

struct ElementRecord: Codable {
    let index: Int
    let parent: Int?
    let depth: Int
    let role: String?
    let subrole: String?
    let title: String?
    let description: String?
    let value: String?
    let help: String?
    let identifier: String?
    let url: String?
    let document: String?
    let actions: [String]
}

struct WindowRecord: Codable {
    let id: UInt32
    let pid: Int32
    let layer: Int
    let x: Double
    let y: Double
    let width: Double
    let height: Double
}

struct Node {
    let element: AXUIElement
    let record: ElementRecord
}

func fail(_ message: String, code: Int32 = 2) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(code)
}

func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    let error = AXUIElementCopyAttributeValue(element, name as CFString, &value)
    return error == .success ? value : nil
}

func text(_ element: AXUIElement, _ name: String) -> String? {
    guard let value = attribute(element, name) else { return nil }
    if let string = value as? String { return string }
    if let url = value as? URL { return url.absoluteString }
    if let attributed = value as? NSAttributedString { return attributed.string }
    if let number = value as? NSNumber { return number.stringValue }
    return nil
}

func children(_ element: AXUIElement) -> [AXUIElement] {
    guard let value = attribute(element, kAXChildrenAttribute as String) else { return [] }
    return value as? [AXUIElement] ?? []
}

func actions(_ element: AXUIElement) -> [String] {
    var names: CFArray?
    guard AXUIElementCopyActionNames(element, &names) == .success,
          let result = names as? [String] else { return [] }
    return result.sorted()
}

func collect(pid: pid_t, limit: Int = 12_000) -> [Node] {
    let app = AXUIElementCreateApplication(pid)
    var queue: [(AXUIElement, Int?, Int)] = [(app, nil, 0)]
    var cursor = 0
    var result: [Node] = []
    let windows = (attribute(app, kAXWindowsAttribute as String) as? [AXUIElement]) ?? []
    while cursor < queue.count {
        if result.count >= limit { fail("AX tree exceeded node limit \(limit)") }
        let (element, parent, depth) = queue[cursor]
        cursor += 1
        let index = result.count
        let record = ElementRecord(
            index: index,
            parent: parent,
            depth: depth,
            role: text(element, kAXRoleAttribute as String),
            subrole: text(element, kAXSubroleAttribute as String),
            title: text(element, kAXTitleAttribute as String),
            description: text(element, kAXDescriptionAttribute as String),
            value: text(element, kAXValueAttribute as String),
            help: text(element, kAXHelpAttribute as String),
            identifier: text(element, "AXIdentifier") ?? text(element, "AXDOMIdentifier"),
            url: text(element, "AXURL"),
            document: text(element, "AXDocument"),
            actions: actions(element)
        )
        result.append(Node(element: element, record: record))
        if depth < 40 {
            let descendants = depth == 0 ? windows : children(element)
            for child in descendants {
                queue.append((child, index, depth + 1))
            }
        }
    }
    return result
}

func searchableText(_ record: ElementRecord) -> String {
    [record.role, record.subrole, record.title, record.description, record.value,
     record.help, record.identifier, record.url, record.document]
        .compactMap { $0 }
        .joined(separator: "\n")
        .lowercased()
}

func encode<T: Encodable>(_ value: T) {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    guard let data = try? encoder.encode(value) else { fail("could not encode JSON") }
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
}

func parsePid(_ raw: String?) -> pid_t {
    guard let raw, let value = Int32(raw), value > 1 else { fail("expected a positive PID") }
    return value
}

func exactWindow(pid: pid_t) -> WindowRecord {
    guard let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
            as? [[String: Any]] else { fail("could not read window metadata") }
    let matches: [WindowRecord] = info.compactMap { item in
        guard let ownerPid = item[kCGWindowOwnerPID as String] as? NSNumber,
              ownerPid.int32Value == pid,
              let number = item[kCGWindowNumber as String] as? NSNumber,
              let layer = item[kCGWindowLayer as String] as? NSNumber,
              layer.intValue == 0,
              let bounds = item[kCGWindowBounds as String] as? [String: Any],
              let x = bounds["X"] as? NSNumber,
              let y = bounds["Y"] as? NSNumber,
              let width = bounds["Width"] as? NSNumber,
              let height = bounds["Height"] as? NSNumber,
              width.doubleValue > 0,
              height.doubleValue > 0 else { return nil }
        return WindowRecord(id: number.uint32Value, pid: ownerPid.int32Value,
                            layer: layer.intValue, x: x.doubleValue, y: y.doubleValue,
                            width: width.doubleValue, height: height.doubleValue)
    }.sorted { ($0.width * $0.height) > ($1.width * $1.height) }
    guard let first = matches.first else { fail("no on-screen layer-zero window for PID \(pid)") }
    return first
}

let arguments = Array(CommandLine.arguments.dropFirst())
guard let command = arguments.first else {
    fail("usage: dakia-ax trust | snapshot PID | press PID QUERY | activate PID | window PID")
}

switch command {
case "trust":
    encode(["accessibility": AXIsProcessTrusted(),
            "screenCapture": CGPreflightScreenCaptureAccess()])
case "snapshot":
    let pid = parsePid(arguments.count > 1 ? arguments[1] : nil)
    encode(collect(pid: pid).map(\.record))
case "press":
    let pid = parsePid(arguments.count > 1 ? arguments[1] : nil)
    guard arguments.count > 2 else { fail("press requires a non-empty query") }
    let query = arguments.dropFirst(2).joined(separator: " ").lowercased()
    guard !query.isEmpty else { fail("press requires a non-empty query") }
    let nodes = collect(pid: pid)
    let direct = nodes.filter {
        $0.record.actions.contains(kAXPressAction as String) && searchableText($0.record).contains(query)
    }
    var candidates = direct
    if candidates.isEmpty {
        var ancestorIndexes = Set<Int>()
        for node in nodes where searchableText(node.record).contains(query) {
            var parent = node.record.parent
            while let index = parent {
                if nodes[index].record.actions.contains(kAXPressAction as String) {
                    ancestorIndexes.insert(index)
                    break
                }
                parent = nodes[index].record.parent
            }
        }
        candidates = ancestorIndexes.sorted().map { nodes[$0] }
    }
    guard candidates.count == 1 else {
        encode(candidates.map(\.record))
        fail("expected exactly one press target for query \(query), found \(candidates.count)")
    }
    let error = AXUIElementPerformAction(candidates[0].element, kAXPressAction as CFString)
    guard error == .success else { fail("AXPress failed with error \(error.rawValue)") }
    encode(candidates[0].record)
case "activate":
    let pid = parsePid(arguments.count > 1 ? arguments[1] : nil)
    guard let app = NSRunningApplication(processIdentifier: pid) else { fail("no running app for PID \(pid)") }
    guard app.activate(options: []) else { fail("could not activate PID \(pid)") }
    encode(["activatedPid": Int(pid)])
case "window":
    let pid = parsePid(arguments.count > 1 ? arguments[1] : nil)
    encode(exactWindow(pid: pid))
default:
    fail("unknown command \(command)")
}
