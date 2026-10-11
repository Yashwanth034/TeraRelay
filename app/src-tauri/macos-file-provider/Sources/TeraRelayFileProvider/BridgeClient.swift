import Darwin
import FileProvider
import Foundation

struct BridgeEndpoint: Decodable {
    let protocolVersion: UInt32
    let host: String
    let port: UInt16

    private enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol"
        case host
        case port
    }
}

struct BridgeItem: Decodable {
    let id: String
    let parent: String
    let name: String
    let kind: String
    let size: UInt64
    let createdAtMs: Int64
    let updatedAtMs: Int64
    let contentVersion: String
    let pending: Bool

    private enum CodingKeys: String, CodingKey {
        case id, parent, name, kind, size, pending
        case createdAtMs = "created_at_ms"
        case updatedAtMs = "updated_at_ms"
        case contentVersion = "content_version"
    }
}

private struct BridgeResponse: Decodable {
    let ok: Bool
    let error: String?
    let revision: UInt64?
    let item: BridgeItem?
    let items: [BridgeItem]?
    let dataB64: String?
    let written: UInt64?
    let nextCursor: UInt64?

    private enum CodingKeys: String, CodingKey {
        case ok, error, revision, item, items, written
        case dataB64 = "data_b64"
        case nextCursor = "next_cursor"
    }
}

enum BridgeError: LocalizedError {
    case unavailable(String)
    case rejected(String)
    case invalidResponse(String)

    var errorDescription: String? {
        switch self {
        case .unavailable(let message), .rejected(let message), .invalidResponse(let message):
            return message
        }
    }
}

final class BridgeClient {
    static let appGroup = "group.io.terarelay.drive"
    static let rootWireIdentifier = Data([0x54, 0x52, 0x44, 0x31, 0x00, 0x00, 0x00]).base64EncodedString()
    static let maxFrameBytes = 8 * 1024 * 1024
    static let maxRequestFrameBytes = 8 * 1024 * 1024
    static let maxContentChunkBytes = 4 * 1024 * 1024

    static let shared = BridgeClient()

    private let fileManager = FileManager.default

    private init() {}

    private func containerURL() throws -> URL {
        guard let url = fileManager.containerURL(forSecurityApplicationGroupIdentifier: Self.appGroup) else {
            throw BridgeError.unavailable("TeraRelay File Provider App Group container is unavailable")
        }
        return url
    }

    private func endpointAndCapability() throws -> (BridgeEndpoint, String) {
        let root = try containerURL()
        let endpointURL = root.appendingPathComponent("provider-endpoint.json", isDirectory: false)
        let capabilityURL = root.appendingPathComponent("provider-capability", isDirectory: false)
        let endpointData: Data
        let capability: String
        do {
            endpointData = try Data(contentsOf: endpointURL)
            capability = try String(contentsOf: capabilityURL, encoding: .utf8)
                .trimmingCharacters(in: .whitespacesAndNewlines)
        } catch {
            throw BridgeError.unavailable("Open TeraRelay before using its Finder location")
        }
        let endpoint = try JSONDecoder().decode(BridgeEndpoint.self, from: endpointData)
        guard endpoint.protocolVersion == 1, endpoint.host == "127.0.0.1", !capability.isEmpty else {
            throw BridgeError.invalidResponse("TeraRelay File Provider endpoint is invalid")
        }
        return (endpoint, capability)
    }

    private func connect(endpoint: BridgeEndpoint) throws -> Int32 {
        let descriptor = Darwin.socket(AF_INET, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw BridgeError.unavailable("Could not create TeraRelay File Provider socket")
        }

        var noSigPipe: Int32 = 1
        let noSigPipeResult = withUnsafePointer(to: &noSigPipe) { pointer in
            Darwin.setsockopt(descriptor, SOL_SOCKET, SO_NOSIGPIPE, pointer, socklen_t(MemoryLayout<Int32>.size))
        }
        guard noSigPipeResult == 0 else {
            Darwin.close(descriptor)
            throw BridgeError.unavailable("Could not configure TeraRelay File Provider socket")
        }

        // Bound only request-frame sends. A response can legitimately take much
        // longer than 30 seconds because write/flush/closeWrite may upload real
        // Drive parts to Telegram. A fixed receive timeout can race a successful
        // host commit with File Provider retry/rollback, so completion is driven
        // by the host socket closing or returning a response instead.
        var sendTimeout = timeval(tv_sec: 30, tv_usec: 0)
        withUnsafePointer(to: &sendTimeout) { pointer in
            _ = Darwin.setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, pointer, socklen_t(MemoryLayout<timeval>.size))
        }

        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = endpoint.port.bigEndian
        let parsed = endpoint.host.withCString { pointer in
            Darwin.inet_pton(AF_INET, pointer, &address.sin_addr)
        }
        guard parsed == 1 else {
            Darwin.close(descriptor)
            throw BridgeError.invalidResponse("TeraRelay File Provider endpoint address is invalid")
        }

        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { socketAddress in
                Darwin.connect(descriptor, socketAddress, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard result == 0 else {
            Darwin.close(descriptor)
            throw BridgeError.unavailable("TeraRelay is not currently available")
        }
        return descriptor
    }

    private func sendAll(_ descriptor: Int32, _ data: Data) throws {
        var sent = 0
        while sent < data.count {
            let count = data.withUnsafeBytes { rawBuffer -> Int in
                guard let base = rawBuffer.baseAddress else { return -1 }
                return Darwin.send(descriptor, base.advanced(by: sent), data.count - sent, 0)
            }
            guard count > 0 else {
                throw BridgeError.unavailable("TeraRelay File Provider request send failed")
            }
            sent += count
        }
    }

    private func receiveExactly(_ descriptor: Int32, count: Int) throws -> Data {
        guard count >= 0, count <= Self.maxFrameBytes else {
            throw BridgeError.invalidResponse("TeraRelay File Provider response is too large")
        }
        var output = Data(count: count)
        var received = 0
        try output.withUnsafeMutableBytes { rawBuffer in
            guard let base = rawBuffer.baseAddress else { return }
            while received < count {
                let amount = Darwin.recv(descriptor, base.advanced(by: received), count - received, 0)
                guard amount > 0 else {
                    throw BridgeError.unavailable("TeraRelay File Provider response ended early")
                }
                received += amount
            }
        }
        return output
    }

    private func call(op: String, fields: [String: Any] = [:]) throws -> BridgeResponse {
        let (endpoint, capability) = try endpointAndCapability()
        var request = fields
        request["protocol"] = 1
        request["capability"] = capability
        request["op"] = op
        let payload = try JSONSerialization.data(withJSONObject: request, options: [])
        guard !payload.isEmpty, payload.count <= Self.maxRequestFrameBytes else {
            throw BridgeError.invalidResponse("TeraRelay File Provider request is too large")
        }

        let descriptor = try connect(endpoint: endpoint)
        defer { Darwin.close(descriptor) }
        var length = UInt32(payload.count).bigEndian
        let header = Data(bytes: &length, count: MemoryLayout<UInt32>.size)
        try sendAll(descriptor, header)
        try sendAll(descriptor, payload)

        let responseHeader = try receiveExactly(descriptor, count: 4)
        guard responseHeader.count == 4 else {
            throw BridgeError.invalidResponse("TeraRelay File Provider response header is incomplete")
        }
        let responseLength = responseHeader.reduce(UInt32(0)) { value, byte in
            (value << 8) | UInt32(byte)
        }
        guard responseLength > 0, responseLength <= Self.maxFrameBytes else {
            throw BridgeError.invalidResponse("TeraRelay File Provider returned an invalid response length")
        }
        let responseData = try receiveExactly(descriptor, count: Int(responseLength))
        let response = try JSONDecoder().decode(BridgeResponse.self, from: responseData)
        guard response.ok else {
            throw BridgeError.rejected(response.error ?? "TeraRelay File Provider operation failed")
        }
        return response
    }

    func wireIdentifier(_ identifier: NSFileProviderItemIdentifier) -> String {
        identifier == .rootContainer ? Self.rootWireIdentifier : identifier.rawValue
    }

    func fileProviderIdentifier(_ wireIdentifier: String) -> NSFileProviderItemIdentifier {
        wireIdentifier == Self.rootWireIdentifier ? .rootContainer : NSFileProviderItemIdentifier(wireIdentifier)
    }

    func revision() throws -> UInt64 {
        guard let revision = try call(op: "revision").revision else {
            throw BridgeError.invalidResponse("TeraRelay File Provider revision response is incomplete")
        }
        return revision
    }

    func metadata(_ identifier: NSFileProviderItemIdentifier) throws -> BridgeItem {
        let response = try call(op: "metadata", fields: ["id": wireIdentifier(identifier)])
        guard let item = response.item else {
            throw BridgeError.invalidResponse("TeraRelay File Provider metadata response is incomplete")
        }
        return item
    }

    func lookup(parent: NSFileProviderItemIdentifier, name: String) throws -> BridgeItem {
        let response = try call(op: "lookup", fields: [
            "parent": wireIdentifier(parent),
            "name": name,
        ])
        guard let item = response.item else {
            throw BridgeError.invalidResponse("TeraRelay File Provider lookup response is incomplete")
        }
        return item
    }

    func list(
        _ identifier: NSFileProviderItemIdentifier,
        cursor: UInt64? = nil
    ) throws -> ([BridgeItem], UInt64, UInt64?) {
        var fields: [String: Any] = ["parent": wireIdentifier(identifier)]
        if let cursor {
            fields["cursor"] = cursor
        }
        let response = try call(op: "list", fields: fields)
        guard let items = response.items, let revision = response.revision else {
            throw BridgeError.invalidResponse("TeraRelay File Provider directory response is incomplete")
        }
        return (items, revision, response.nextCursor)
    }

    func read(
        _ identifier: NSFileProviderItemIdentifier,
        contentVersion: String,
        offset: UInt64,
        length: UInt64
    ) throws -> Data {
        guard length <= UInt64(Self.maxContentChunkBytes) else {
            throw BridgeError.invalidResponse("TeraRelay File Provider read exceeds the chunk limit")
        }
        let response = try call(op: "read", fields: [
            "id": wireIdentifier(identifier),
            "content_version": contentVersion,
            "offset": offset,
            "length": length,
        ])
        guard let encoded = response.dataB64, let data = Data(base64Encoded: encoded) else {
            throw BridgeError.invalidResponse("TeraRelay File Provider returned invalid content")
        }
        return data
    }

    func createFile(parent: NSFileProviderItemIdentifier, name: String) throws -> BridgeItem {
        let response = try call(op: "createFile", fields: [
            "parent": wireIdentifier(parent),
            "name": name,
        ])
        guard let item = response.item else {
            throw BridgeError.invalidResponse("TeraRelay File Provider create-file response is incomplete")
        }
        return item
    }

    func createDirectory(parent: NSFileProviderItemIdentifier, name: String) throws -> BridgeItem {
        let response = try call(op: "createDirectory", fields: [
            "parent": wireIdentifier(parent),
            "name": name,
        ])
        guard let item = response.item else {
            throw BridgeError.invalidResponse("TeraRelay File Provider create-directory response is incomplete")
        }
        return item
    }

    func beginWrite(_ identifier: NSFileProviderItemIdentifier) throws -> BridgeItem {
        let response = try call(op: "beginWrite", fields: ["id": wireIdentifier(identifier)])
        guard let item = response.item else {
            throw BridgeError.invalidResponse("TeraRelay File Provider begin-write response is incomplete")
        }
        return item
    }

    @discardableResult
    func write(_ identifier: NSFileProviderItemIdentifier, offset: UInt64, data: Data) throws -> UInt64 {
        guard data.count <= Self.maxContentChunkBytes else {
            throw BridgeError.invalidResponse("TeraRelay File Provider write exceeds the chunk limit")
        }
        let response = try call(op: "write", fields: [
            "id": wireIdentifier(identifier),
            "offset": offset,
            "data_b64": data.base64EncodedString(),
        ])
        guard let written = response.written else {
            throw BridgeError.invalidResponse("TeraRelay File Provider write response is incomplete")
        }
        return written
    }

    func resize(_ identifier: NSFileProviderItemIdentifier, size: UInt64) throws {
        _ = try call(op: "resize", fields: ["id": wireIdentifier(identifier), "size": size])
    }

    func flush(_ identifier: NSFileProviderItemIdentifier) throws {
        _ = try call(op: "flush", fields: ["id": wireIdentifier(identifier)])
    }

    func closeWrite(_ identifier: NSFileProviderItemIdentifier) throws {
        _ = try call(op: "closeWrite", fields: ["id": wireIdentifier(identifier)])
    }

    func abortWrite(_ identifier: NSFileProviderItemIdentifier) throws {
        _ = try call(op: "abortWrite", fields: ["id": wireIdentifier(identifier)])
    }

    func rename(_ identifier: NSFileProviderItemIdentifier, parent: NSFileProviderItemIdentifier, name: String) throws -> BridgeItem {
        let response = try call(op: "rename", fields: [
            "id": wireIdentifier(identifier),
            "parent": wireIdentifier(parent),
            "name": name,
        ])
        guard let item = response.item else {
            throw BridgeError.invalidResponse("TeraRelay File Provider rename response is incomplete")
        }
        return item
    }

    func remove(_ identifier: NSFileProviderItemIdentifier) throws {
        _ = try call(op: "remove", fields: ["id": wireIdentifier(identifier)])
    }

    func removeRecursively(_ identifier: NSFileProviderItemIdentifier) throws {
        _ = try call(op: "removeRecursive", fields: ["id": wireIdentifier(identifier)])
    }
}
