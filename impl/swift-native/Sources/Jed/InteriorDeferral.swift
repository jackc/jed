/// Allocation-independent deferral/fold decision. Published tree nodes are never
/// changed: serialization builds a new resident skeleton over retained pages.
internal struct InteriorDeferral: Sendable {
    var entries: [PageRemap] = []
    var retained: Set<UInt32> = []
    var rewrite: Set<UInt32> = []
    var folding = false
    var catalog: CatalogImage?
    var catalogRoot: PageReference?
    var catalogPages: [UInt32] = []

    static func trees(_ snapshot: DomainSnapshot) -> [BPlusTree] {
        snapshot.tables.keys.sorted().flatMap { name -> [BPlusTree] in
            let table = snapshot.catalog.tables.first { $0.name == name }!
            return [snapshot.tables[name]!.tree] + table.indexes.filter { $0.kind != 2 }
                .sorted { $0.name < $1.name }.map { snapshot.indexes[$0.name]!.tree }
        }
    }

    static func plan(_ snapshot: DomainSnapshot, previous: DomainStorageState, meta: MetaPage,
                     free: [UInt32], enabled: Bool, synchronous: Bool) -> InteriorDeferral {
        var result = InteriorDeferral(entries: meta.remaps)
        var kept: Set<UInt32> = [], additions: Set<RemapKey> = [], dirty = false
        let migrate = meta.pageCount >= 3 && free.last != meta.pageCount - 1 && free.contains(where: { $0 < meta.pageCount - 1 })
            ? meta.pageCount - 1 : 0
        func visit(_ child: TreeChild) -> Bool {
            if let reference = child.reference { kept.insert(reference.index); return false }
            dirty = true
            guard case .resident(let node) = child, !node.isLeaf else { return true }
            var moved: [Int] = []
            for (slot, child) in node.children.enumerated() { if visit(child) { moved.append(slot) } }
            guard enabled, let origin = node.origin, origin.index != migrate else { return true }
            result.retained.insert(origin.index)
            for slot in moved { additions.insert(RemapKey(parent: origin.index, slot: UInt16(slot))) }
            return false
        }
        for tree in trees(snapshot) { if let root = tree.root { _ = visit(root) } }
        var retired: Set<UInt32> = []
        func retire(_ child: TreeChild) {
            guard let reference = child.reference, !kept.contains(reference.index),
                  case .resident(let node) = child, !node.isLeaf else { return }
            if !result.retained.contains(reference.index) { retired.insert(reference.index) }
            for child in node.children { retire(child) }
        }
        for tree in trees(previous.snapshot) { if let root = tree.root { retire(root) } }
        result.entries.removeAll { retired.contains($0.parent) }
        let keys = Set(result.entries.map { RemapKey(parent: $0.parent, slot: $0.slot) }).union(additions)
        if !enabled || keys.count > PageRemap.capacity(pageSize: meta.pageSize, encrypted: meta.encrypted) {
            result.folding = true; result.entries = []; result.retained = []
            let stale = Set(meta.remaps.map(\.parent))
            func fold(_ child: TreeChild) -> Bool {
                guard case .resident(let node) = child, !node.isLeaf else { return false }
                var changed = node.reference.map { stale.contains($0.index) } ?? false
                for child in node.children { if fold(child) { changed = true } }
                if changed, let reference = node.reference { result.rewrite.insert(reference.index) }
                return changed
            }
            for tree in trees(snapshot) { if let root = tree.root { _ = fold(root) } }
        } else if previous.catalogKnown && meta.root.index != migrate && (synchronous || dirty) {
            result.catalog = previous.snapshot.catalog; result.catalogRoot = meta.root
            result.catalogPages = previous.catalogPages
        }
        return result
    }

    func keepsCatalog(_ image: CatalogImage, codec: any PageCodec) throws -> Bool {
        guard let catalog else { return false }
        let old = try catalog.entries().map { try CatalogCodec.encode($0, encrypted: codec.reservedBytes != 0) }
        let new = try image.entries().map { try CatalogCodec.encode($0, encrypted: codec.reservedBytes != 0) }
        return old == new
    }
}

internal struct RemapKey: Hashable, Sendable {
    let parent: UInt32
    let slot: UInt16
}
