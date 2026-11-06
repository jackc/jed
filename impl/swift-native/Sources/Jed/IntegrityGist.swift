internal struct IntegrityGistItem {
    let raw: [UInt8]
    let bound: [GistBound]
    let key: [UInt8]
    let child: PageReference
}
internal extension IntegrityChecker {
    func gistOps(table: CatalogTable, index: CatalogIndex) throws -> [GistOpclass] {
        let types = try snapshot.tableTypes(table)
        return try index.keys.map {
            guard case .column(let ordinal) = $0, types.indices.contains(ordinal) else { throw JedError.corruption("invalid GiST column") }
            return try GistOpclass(type: types[ordinal])
        }
    }
    func gistItems(_ bytes: [UInt8], ops: [GistOpclass]) throws -> [IntegrityGistItem] {
        let count = Int(try PageEnvelope.readU32(bytes, at: 4)), leaf = bytes[0] == 5
        guard count <= (bytes.count - 16) / 2 else { throw JedError.corruption("GiST count overruns page") }
        var reader = ByteReader(bytes[16...]), result: [IntegrityGistItem] = []
        for _ in 0..<count {
            let size = Int(try reader.u16()), raw = Array(try reader.take(size))
            var boundReader = ByteReader(raw)
            let bound = try GistTuple.decode(&boundReader, ops: ops, leaf: leaf)
            try boundReader.requireEnd()
            let key: [UInt8], child: PageReference
            if leaf { let size = Int(try reader.u16()); key = Array(try reader.take(size)); child = PageSealer.nullReference(codec) }
            else { key = []; child = PageReference(try reader.u32(), tag: meta.encrypted ? Array(try reader.take(16)) : []) }
            result.append(IntegrityGistItem(raw: raw, bound: bound, key: key, child: child))
        }
        return result
    }
    func gistContains(_ outer: [GistBound], _ inner: [GistBound], ops: [GistOpclass]) throws -> Bool {
        let union = try GistTuple.union(outer, inner, ops: ops)
        return try GistTuple.compare(union, outer, ops: ops) == 0
    }
    func walkGist(root: PageReference?, table: CatalogTable, index: CatalogIndex) throws -> IntegrityTreeResult {
        guard let root else { return IntegrityTreeResult(unreadable: true) }
        let ops = try gistOps(table: table, index: index)
        var result = IntegrityTreeResult(), leafDepth: Int?
        func visit(_ reference: PageReference, depth: Int, parent: [GistBound]?) throws {
            guard try reach(reference, object: index.name) else { result.unreadable = true; return }
            guard depth < 256 else { result.unreadable = true; try add("page_corrupt", index.name, page: reference.index); return }
            let bytes: [UInt8], items: [IntegrityGistItem]
            do {
                bytes = try body(reference, cached: false)
                guard bytes[0] == 5 || bytes[0] == 6 else {
                    result.unreadable = true; try add("page_type_wrong", index.name, page: reference.index); return
                }
                items = try gistItems(bytes, ops: ops)
            } catch let error as JedError where error.code == "XX001" {
                result.unreadable = true; try add("page_corrupt", index.name, page: reference.index, error.message); return
            }
            if bytes[0] == 5 {
                if let leafDepth, leafDepth != depth { try add("tree_depth_uneven", index.name, page: reference.index) }
                else if leafDepth == nil { leafDepth = depth }
            }
            if let parent {
                for item in items where try !gistContains(parent, item.bound, ops: ops) {
                    try add("gist_bounds_violation", index.name, page: reference.index); break
                }
            }
            if bytes[0] == 5 { result.records += UInt64(items.count) }
            else { for item in items { try visit(item.child, depth: depth + 1, parent: item.bound) } }
        }
        if root.index != 0 { try visit(root, depth: 0, parent: nil) }
        return result
    }
    func gistHolds(_ entry: StoredIndexPlan.Entry, root: PageReference, ops: [GistOpclass]) throws -> Bool {
        var reader = ByteReader(entry.term)
        let bound = try GistTuple.decode(&reader, ops: ops, leaf: true)
        try reader.requireEnd()
        func visit(_ reference: PageReference, depth: Int) throws -> Bool {
            guard depth < 256 else { throw JedError.corruption("excessive GiST lookup depth") }
            let bytes = try body(reference, cached: false)
            guard bytes[0] == 5 || bytes[0] == 6 else { throw JedError.corruption("invalid GiST lookup page") }
            for item in try gistItems(bytes, ops: ops) {
                if bytes[0] == 5 {
                    if item.raw == entry.term && item.key == entry.storageKey { return true }
                } else if try gistContains(item.bound, bound, ops: ops), try visit(item.child, depth: depth + 1) { return true }
            }
            return false
        }
        if root.index == 0 { return false }
        do { return try visit(root, depth: 0) }
        catch let error as JedError where error.code == "XX001" { return false }
    }
}
