import FileProvider
import Foundation

final class TeraRelayFileProviderEnumerator: NSObject, NSFileProviderEnumerator {
    private static let maxWorkingSetDepth = 512
    private static let maxWorkingSetItems = 100_000
    private static let enumerationBatchSize = 512

    private let containerIdentifier: NSFileProviderItemIdentifier
    private let isWorkingSet: Bool
    private let bridge: BridgeClient
    private let queue = DispatchQueue(label: "io.terarelay.fileprovider.enumerator", qos: .utility)
    private let invalidated = NSLock()
    private var isInvalidated = false

    init(containerIdentifier: NSFileProviderItemIdentifier, bridge: BridgeClient = .shared) {
        self.isWorkingSet = containerIdentifier == .workingSet
        self.containerIdentifier = containerIdentifier == .workingSet ? .rootContainer : containerIdentifier
        self.bridge = bridge
        super.init()
    }

    func invalidate() {
        invalidated.lock()
        isInvalidated = true
        invalidated.unlock()
    }

    private func active() -> Bool {
        invalidated.lock()
        defer { invalidated.unlock() }
        return !isInvalidated
    }

    private func anchor(_ revision: UInt64) -> NSFileProviderSyncAnchor {
        var encoded = revision.bigEndian
        return withUnsafeBytes(of: &encoded) { raw in
            NSFileProviderSyncAnchor(Data(raw))
        }
    }

    private func revision(from anchor: NSFileProviderSyncAnchor) -> UInt64? {
        let bytes = [UInt8](anchor.rawValue)
        guard bytes.count == MemoryLayout<UInt64>.size else { return nil }
        return bytes.reduce(UInt64(0)) { value, byte in
            (value << 8) | UInt64(byte)
        }
    }

    private func enumerateDirectory(
        _ identifier: NSFileProviderItemIdentifier,
        observer: NSFileProviderEnumerationObserver
    ) throws {
        var cursor: UInt64? = 0
        var expectedRevision: UInt64?
        while cursor != nil {
            guard active() else { throw CocoaError(.userCancelled) }
            let (wireItems, revision, nextCursor) = try bridge.list(identifier, cursor: cursor)
            if let expectedRevision, expectedRevision != revision {
                throw NSFileProviderError(.syncAnchorExpired)
            }
            expectedRevision = expectedRevision ?? revision
            let items: [NSFileProviderItem] = wireItems.map {
                TeraRelayFileProviderItem(wire: $0, bridge: bridge)
            }
            if !items.isEmpty {
                observer.didEnumerate(items)
            }
            cursor = nextCursor
        }
    }

    private func enumerateWorkingSet(observer: NSFileProviderEnumerationObserver) throws {
        struct PendingDirectory {
            let identifier: NSFileProviderItemIdentifier
            let depth: Int
        }

        var pending = [PendingDirectory(identifier: .rootContainer, depth: 0)]
        var cursor = 0
        var expectedRevision: UInt64?
        var itemCount = 0
        var batch: [NSFileProviderItem] = []
        batch.reserveCapacity(Self.enumerationBatchSize)

        while cursor < pending.count {
            guard active() else {
                throw CocoaError(.userCancelled)
            }
            let current = pending[cursor]
            cursor += 1
            guard current.depth <= Self.maxWorkingSetDepth else {
                throw BridgeError.invalidResponse("TeraRelay working-set hierarchy is too deep")
            }

            var pageCursor: UInt64? = 0
            while pageCursor != nil {
                let (wireItems, revision, nextCursor) = try bridge.list(
                    current.identifier,
                    cursor: pageCursor
                )
                if let expectedRevision, expectedRevision != revision {
                    // Never hand File Provider a snapshot assembled across two metadata
                    // revisions. The system will retry from a fresh anchor.
                    throw NSFileProviderError(.syncAnchorExpired)
                }
                expectedRevision = expectedRevision ?? revision

                for wire in wireItems {
                    itemCount += 1
                    guard itemCount <= Self.maxWorkingSetItems else {
                        throw BridgeError.invalidResponse("TeraRelay working set exceeds the safe item limit")
                    }
                    let item = TeraRelayFileProviderItem(wire: wire, bridge: bridge)
                    batch.append(item)
                    if wire.kind == "directory" {
                        pending.append(PendingDirectory(
                            identifier: bridge.fileProviderIdentifier(wire.id),
                            depth: current.depth + 1
                        ))
                    }
                    if batch.count >= Self.enumerationBatchSize {
                        observer.didEnumerate(batch)
                        batch.removeAll(keepingCapacity: true)
                    }
                }
                pageCursor = nextCursor
            }
        }

        if !batch.isEmpty {
            observer.didEnumerate(batch)
        }
        let finalRevision = try bridge.revision()
        if let expectedRevision, finalRevision != expectedRevision {
            throw NSFileProviderError(.syncAnchorExpired)
        }
    }

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        queue.async { [weak self] in
            guard let self, self.active() else {
                observer.finishEnumerating(upTo: nil)
                return
            }
            do {
                if self.isWorkingSet {
                    try self.enumerateWorkingSet(observer: observer)
                } else {
                    try self.enumerateDirectory(self.containerIdentifier, observer: observer)
                }
                observer.finishEnumerating(upTo: nil)
            } catch {
                observer.finishEnumeratingWithError(error)
            }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        queue.async { [weak self] in
            guard let self, self.active() else {
                completionHandler(nil)
                return
            }
            do {
                completionHandler(self.anchor(try self.bridge.revision()))
            } catch {
                completionHandler(nil)
            }
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from syncAnchor: NSFileProviderSyncAnchor) {
        queue.async { [weak self] in
            guard let self, self.active() else {
                observer.finishEnumeratingWithError(NSFileProviderError(.providerNotFound))
                return
            }
            do {
                let current = try self.bridge.revision()
                guard self.revision(from: syncAnchor) == current else {
                    // The shared Rust metadata revision is intentionally the single
                    // source of truth. Expire a stale anchor so macOS performs one
                    // complete authoritative working-set re-enumeration; that snapshot
                    // includes nested items and therefore covers remote move/delete too.
                    observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
                    return
                }
                observer.finishEnumeratingChanges(upTo: self.anchor(current), moreComing: false)
            } catch {
                observer.finishEnumeratingWithError(error)
            }
        }
    }
}
