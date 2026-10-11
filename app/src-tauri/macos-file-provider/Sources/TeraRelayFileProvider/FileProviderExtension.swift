import FileProvider
import Foundation
import UniformTypeIdentifiers

final class TeraRelayFileProviderExtension: NSObject, NSFileProviderReplicatedExtension, NSFileProviderEnumerating {
    private let domain: NSFileProviderDomain
    private let bridge = BridgeClient.shared
    private let queue = DispatchQueue(label: "io.terarelay.fileprovider.operations", qos: .utility, attributes: .concurrent)

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        super.init()
    }

    func invalidate() {}

    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        TeraRelayFileProviderEnumerator(containerIdentifier: containerItemIdentifier, bridge: bridge)
    }

    func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        queue.async { [bridge] in
            guard !progress.isCancelled else {
                completionHandler(nil, CocoaError(.userCancelled))
                return
            }
            do {
                let wire = try bridge.metadata(identifier)
                completionHandler(TeraRelayFileProviderItem(wire: wire, bridge: bridge), nil)
                progress.completedUnitCount = 1
            } catch {
                completionHandler(nil, self.providerError(error))
            }
        }
        return progress
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 100)
        queue.async { [bridge] in
            var outputURL: URL?
            do {
                let wire = try bridge.metadata(itemIdentifier)
                let item = TeraRelayFileProviderItem(wire: wire, bridge: bridge)
                if let requestedVersion, requestedVersion != item.itemVersion {
                    throw NSFileProviderError(.versionOutOfDate)
                }
                guard wire.kind == "file" else {
                    throw NSFileProviderError(.noSuchItem)
                }
                guard let manager = NSFileProviderManager(for: self.domain) else {
                    throw NSFileProviderError(.providerNotFound)
                }
                let url = try manager.temporaryDirectoryURL()
                    .appendingPathComponent("terarelay-fileprovider-\(UUID().uuidString)", isDirectory: false)
                FileManager.default.createFile(atPath: url.path, contents: nil)
                let handle = try FileHandle(forWritingTo: url)
                defer { try? handle.close() }
                outputURL = url

                var offset: UInt64 = 0
                let total = max(wire.size, 1)
                while offset < wire.size {
                    if progress.isCancelled {
                        throw CocoaError(.userCancelled)
                    }
                    let length = min(UInt64(BridgeClient.maxContentChunkBytes), wire.size - offset)
                    let bytes = try bridge.read(
                        itemIdentifier,
                        contentVersion: wire.contentVersion,
                        offset: offset,
                        length: length
                    )
                    if bytes.isEmpty && length > 0 {
                        throw BridgeError.invalidResponse("TeraRelay returned an incomplete file")
                    }
                    try handle.write(contentsOf: bytes)
                    offset += UInt64(bytes.count)
                    progress.completedUnitCount = min(99, Int64((offset * 100) / total))
                }
                try handle.synchronize()
                progress.completedUnitCount = 100
                completionHandler(url, item, nil)
            } catch {
                if let outputURL { try? FileManager.default.removeItem(at: outputURL) }
                completionHandler(nil, nil, self.providerError(error))
            }
        }
        return progress
    }

    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 100)
        queue.async { [bridge] in
            do {
                if progress.isCancelled { throw CocoaError(.userCancelled) }
                let existing = options.contains(.mayAlreadyExist)
                    ? try self.lookupExisting(
                        parent: itemTemplate.parentItemIdentifier,
                        name: itemTemplate.filename
                    )
                    : nil
                let wire: BridgeItem
                if itemTemplate.contentType == .folder {
                    if let existing {
                        guard existing.kind == "directory" else {
                            throw NSFileProviderError(.filenameCollision)
                        }
                        wire = existing
                    } else {
                        wire = try bridge.createDirectory(
                            parent: itemTemplate.parentItemIdentifier,
                            name: itemTemplate.filename
                        )
                    }
                } else {
                    let created: BridgeItem
                    if let existing {
                        guard existing.kind == "file" else {
                            throw NSFileProviderError(.filenameCollision)
                        }
                        created = existing
                    } else {
                        created = try bridge.createFile(
                            parent: itemTemplate.parentItemIdentifier,
                            name: itemTemplate.filename
                        )
                    }
                    let identifier = bridge.fileProviderIdentifier(created.id)
                    if existing != nil, url != nil {
                        _ = try bridge.beginWrite(identifier)
                    }
                    if existing == nil || url != nil {
                        try self.uploadContents(url, to: identifier, alreadyWritable: true, progress: progress)
                    }
                    wire = try bridge.metadata(identifier)
                }
                let contentApplied = itemTemplate.contentType != .folder && (existing == nil || url != nil)
                progress.completedUnitCount = 100
                completionHandler(
                    TeraRelayFileProviderItem(wire: wire, bridge: bridge),
                    self.unappliedFields(fields, contentApplied: contentApplied),
                    false,
                    nil
                )
            } catch {
                completionHandler(nil, fields, false, self.providerError(error))
            }
        }
        return progress
    }

    func modifyItem(
        _ item: NSFileProviderItem,
        baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields,
        contents newContents: URL?,
        options: NSFileProviderModifyItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 100)
        queue.async { [bridge] in
            do {
                if progress.isCancelled { throw CocoaError(.userCancelled) }
                let currentWire = try bridge.metadata(item.itemIdentifier)
                let currentItem = TeraRelayFileProviderItem(wire: currentWire, bridge: bridge)
                guard currentItem.itemVersion == version else {
                    throw NSFileProviderError(.versionOutOfDate)
                }
                var identifier = item.itemIdentifier
                let hasContentChanges = changedFields.contains(.contents)
                var pendingWriteAwaitingUpload = false
                do {
                    // Start copy-on-write before changing path metadata. The stable
                    // provider identity then resolves to the pending replacement, so
                    // rename/reparent and content bytes commit together on closeWrite.
                    // If rename itself fails, abortWrite removes the pending row and
                    // leaves the original remote file untouched at its old path.
                    if hasContentChanges {
                        _ = try bridge.beginWrite(identifier)
                        pendingWriteAwaitingUpload = true
                    }
                    if changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier) {
                        let renamed = try bridge.rename(
                            identifier,
                            parent: item.parentItemIdentifier,
                            name: item.filename
                        )
                        identifier = bridge.fileProviderIdentifier(renamed.id)
                    }
                    if hasContentChanges {
                        // uploadContents owns rollback from this point onward.
                        pendingWriteAwaitingUpload = false
                        try self.uploadContents(
                            newContents,
                            to: identifier,
                            alreadyWritable: true,
                            progress: progress
                        )
                    }
                } catch {
                    if pendingWriteAwaitingUpload {
                        try? bridge.abortWrite(identifier)
                    }
                    throw error
                }
                let wire = try bridge.metadata(identifier)
                progress.completedUnitCount = 100
                completionHandler(
                    TeraRelayFileProviderItem(wire: wire, bridge: bridge),
                    self.unappliedFields(changedFields, contentApplied: hasContentChanges),
                    false,
                    nil
                )
            } catch {
                completionHandler(nil, changedFields, false, self.providerError(error))
            }
        }
        return progress
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        queue.async { [bridge] in
            do {
                if progress.isCancelled { throw CocoaError(.userCancelled) }
                let currentWire = try bridge.metadata(identifier)
                let currentItem = TeraRelayFileProviderItem(wire: currentWire, bridge: bridge)
                guard currentItem.itemVersion == version else {
                    throw NSFileProviderError(.versionOutOfDate)
                }
                if options.contains(.recursive) && currentWire.kind == "directory" {
                    try bridge.removeRecursively(identifier)
                } else {
                    try bridge.remove(identifier)
                }
                progress.completedUnitCount = 1
                completionHandler(nil)
            } catch {
                completionHandler(self.providerError(error))
            }
        }
        return progress
    }

    private func unappliedFields(
        _ requested: NSFileProviderItemFields,
        contentApplied: Bool
    ) -> NSFileProviderItemFields {
        var applied: NSFileProviderItemFields = [.filename, .parentItemIdentifier]
        if contentApplied {
            applied.insert(.contents)
        }
        return requested.subtracting(applied)
    }

    private func lookupExisting(
        parent: NSFileProviderItemIdentifier,
        name: String
    ) throws -> BridgeItem? {
        do {
            return try bridge.lookup(parent: parent, name: name)
        } catch {
            let message = error.localizedDescription.lowercased()
            if message.contains("not found") || message.contains("no such") {
                return nil
            }
            throw error
        }
    }

    private func uploadContents(
        _ url: URL?,
        to identifier: NSFileProviderItemIdentifier,
        alreadyWritable: Bool,
        progress: Progress
    ) throws {
        var completed = false
        defer {
            if !completed {
                try? bridge.abortWrite(identifier)
            }
        }
        if !alreadyWritable {
            _ = try bridge.beginWrite(identifier)
        }
        try bridge.resize(identifier, size: 0)

        guard let url else {
            try bridge.flush(identifier)
            try bridge.closeWrite(identifier)
            completed = true
            return
        }

        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        let expectedSize = (attributes[.size] as? NSNumber)?.uint64Value ?? 0
        let total = max(expectedSize, 1)
        var offset: UInt64 = 0

        while true {
            if progress.isCancelled { throw CocoaError(.userCancelled) }
            let data = try handle.read(upToCount: BridgeClient.maxContentChunkBytes) ?? Data()
            if data.isEmpty { break }
            let written = try bridge.write(identifier, offset: offset, data: data)
            guard written == UInt64(data.count) else {
                throw BridgeError.invalidResponse("TeraRelay accepted only part of a File Provider write")
            }
            offset += written
            progress.completedUnitCount = min(99, Int64((offset * 100) / total))
        }
        try bridge.resize(identifier, size: offset)
        try bridge.flush(identifier)
        try bridge.closeWrite(identifier)
        completed = true
    }

    private func providerError(_ error: Error) -> Error {
        if error is NSFileProviderError || (error as NSError).domain == NSCocoaErrorDomain {
            return error
        }
        let description = error.localizedDescription.lowercased()
        if description.contains("not currently available") || description.contains("open terarelay") {
            return NSFileProviderError(.serverUnreachable)
        }
        if description.contains("not found") || description.contains("does not exist") || description.contains("no such") {
            return NSFileProviderError(.noSuchItem)
        }
        if description.contains("already exists") || description.contains("collision") {
            return NSFileProviderError(.filenameCollision)
        }
        if description.contains("not empty") {
            return NSFileProviderError(.directoryNotEmpty)
        }
        return NSFileProviderError(.cannotSynchronize)
    }
}
