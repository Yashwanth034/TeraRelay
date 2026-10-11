import FileProvider
import Foundation
import UniformTypeIdentifiers

final class TeraRelayFileProviderItem: NSObject, NSFileProviderItem {
    private let wire: BridgeItem
    private let bridge: BridgeClient

    init(wire: BridgeItem, bridge: BridgeClient = .shared) {
        self.wire = wire
        self.bridge = bridge
        super.init()
    }

    var itemIdentifier: NSFileProviderItemIdentifier {
        bridge.fileProviderIdentifier(wire.id)
    }

    var parentItemIdentifier: NSFileProviderItemIdentifier {
        bridge.fileProviderIdentifier(wire.parent)
    }

    var filename: String {
        wire.name
    }

    var contentType: UTType {
        if wire.kind == "directory" {
            return .folder
        }
        let ext = URL(fileURLWithPath: wire.name).pathExtension
        if !ext.isEmpty, let inferred = UTType(filenameExtension: ext) {
            return inferred
        }
        return .data
    }

    var capabilities: NSFileProviderItemCapabilities {
        if wire.kind == "directory" {
            return [
                .allowsReading,
                .allowsAddingSubItems,
                .allowsContentEnumerating,
                .allowsRenaming,
                .allowsReparenting,
                .allowsDeleting,
            ]
        }
        return [
            .allowsReading,
            .allowsWriting,
            .allowsRenaming,
            .allowsReparenting,
            .allowsDeleting,
            .allowsEvicting,
        ]
    }

    var documentSize: NSNumber? {
        wire.kind == "file" ? NSNumber(value: wire.size) : nil
    }

    var creationDate: Date? {
        Date(timeIntervalSince1970: TimeInterval(wire.createdAtMs) / 1000.0)
    }

    var contentModificationDate: Date? {
        Date(timeIntervalSince1970: TimeInterval(wire.updatedAtMs) / 1000.0)
    }

    var itemVersion: NSFileProviderItemVersion {
        // Timestamp alone is not a safe conflict token: two metadata changes can
        // land in one millisecond and a content replacement can keep the same size.
        // Bind metadata to its actual path and content to TeraRelay's immutable
        // backing-object/pending identity so stale Finder mutations are rejected.
        let metadata = versionData(
            [UInt64(bitPattern: wire.updatedAtMs), wire.pending ? 1 : 0],
            strings: [wire.parent, wire.name]
        )
        let content = versionData(
            [wire.size],
            strings: [wire.contentVersion]
        )
        return NSFileProviderItemVersion(contentVersion: content, metadataVersion: metadata)
    }

    private func versionData(_ components: [UInt64], strings: [String]) -> Data {
        var data = Data()
        for value in components {
            var encoded = value.bigEndian
            withUnsafeBytes(of: &encoded) { bytes in
                data.append(contentsOf: bytes)
            }
        }
        for string in strings {
            let bytes = Data(string.utf8)
            var length = UInt64(bytes.count).bigEndian
            withUnsafeBytes(of: &length) { raw in
                data.append(contentsOf: raw)
            }
            data.append(bytes)
        }
        return data
    }
}
