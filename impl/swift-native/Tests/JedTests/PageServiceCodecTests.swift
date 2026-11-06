import Foundation
import XCTest
@testable import Jed

final class PageServiceCodecTests: XCTestCase {
    private func document() throws -> [String: Any] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let root = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(root["documents"] as? [String: [String: Any]])
        return try XCTUnwrap(documents["spec/pageservice/fixtures.toml"])
    }
    private func hex(_ text: String) throws -> [UInt8] {
        let bytes = Array(text.utf8)
        guard bytes.count % 2 == 0 else { throw JedError("XX000", "odd hex fixture") }
        return try stride(from: 0, to: bytes.count, by: 2).map {
            try XCTUnwrap(UInt8(String(decoding: bytes[$0..<($0 + 2)], as: UTF8.self), radix: 16))
        }
    }
    private func value(_ row: [String: Any], _ field: String) throws -> String { try XCTUnwrap(row["f_" + field] as? String) }
    private func u64(_ row: [String: Any], _ field: String) throws -> UInt64 { try XCTUnwrap(UInt64(value(row, field))) }
    private func u32(_ row: [String: Any], _ field: String) throws -> UInt32 { try XCTUnwrap(UInt32(value(row, field))) }
    private func u16(_ row: [String: Any], _ field: String) throws -> UInt16 { try XCTUnwrap(UInt16(value(row, field))) }
    private func u8(_ row: [String: Any], _ field: String) throws -> UInt8 { try XCTUnwrap(UInt8(value(row, field))) }
    private func indices(_ text: String) throws -> [UInt32] {
        try text.split(separator: ",").map { try XCTUnwrap(UInt32($0)) }
    }
    private func page(_ text: String, size: Int) throws -> [UInt8] {
        let parts = text.split(separator: "/", omittingEmptySubsequences: false)
        let prefix = try hex(String(parts[0])), fill = try hex(String(parts[1]))
        guard fill.count == 1, prefix.count <= size else { throw JedError("XX000", "invalid page fixture") }
        return prefix + [UInt8](repeating: fill[0], count: size - prefix.count)
    }
    private func indexed(_ text: String, size: Int) throws -> [StoredPage] {
        try text.split(separator: ";").map { item in
            let parts = item.split(separator: "=", maxSplits: 1)
            return try StoredPage(index: XCTUnwrap(UInt32(parts[0])), bytes: page(String(parts[1]), size: size))
        }
    }
    private func point(_ row: [String: Any], txid: String = "txid") throws -> ServiceCachePoint {
        try ServiceCachePoint(incarnation: hex(value(row, "incarnation")), txid: u64(row, txid))
    }
    private func read(_ row: [String: Any], size: Int) throws -> ServiceReadReply {
        try ServiceReadReply(point: point(row), invalidate: u8(row, "invalidate"),
            indices: indices(value(row, "indices")), meta: page(value(row, "meta"), size: size))
    }
    private func message(_ row: [String: Any], size: Int) throws -> ServiceMessage {
        switch try XCTUnwrap(row["message"] as? String) {
        case "hello":
            let text = try value(row, "credential"), credential: Credential?
            if text.isEmpty { credential = nil }
            else if text.hasPrefix("passphrase:") { credential = .passphrase(String(text.dropFirst(11))) }
            else { credential = try .hostKey(hex(String(text.dropFirst(4)))) }
            return .hello(try ServiceHello(minimum: u16(row, "protocol_min"), maximum: u16(row, "protocol_max"),
                format: u16(row, "format_version"), flags: u8(row, "flags"), clientName: value(row, "client_name"), credential: credential))
        case "begin_read": return .beginRead(try point(row, txid: "last_txid"))
        case "read_pages": return try .readPages(txid: u64(row, "txid"), indices: indices(value(row, "indices")))
        case "end_read": return .endRead(try u64(row, "txid"))
        case "begin_write": return try .beginWrite(point(row, txid: "last_txid"), timeout: u64(row, "lock_timeout_ms"), flags: u8(row, "flags"))
        case "commit_begin": return .commitBegin(try ServiceCommitBegin(txid: u64(row, "txid"), meta: page(value(row, "meta"), size: size), manifest: indexed(value(row, "manifest_pages"), size: size)))
        case "commit_pages": return .commitPages(try indexed(value(row, "pages"), size: size))
        case "commit_end": return .commitEnd
        case "abort": return .abort
        case "commit_status": return try .commitStatus(txid: u64(row, "txid"), digest: u64(row, "digest"))
        case "request_ownership": return .requestOwnership
        case "hello_ok": return .helloOK(try ServiceHelloReply(protocolVersion: u16(row, "protocol"), format: u16(row, "format_version"), pageSize: u32(row, "page_size"), connection: u64(row, "connection_id"), flags: u8(row, "flags"), maxReadPages: u32(row, "max_read_pages")))
        case "read_begun": return .readBegun(try read(row, size: size))
        case "pages": return .pages(try value(row, "pages").split(separator: ";").map { try page(String($0), size: size) })
        case "write_begun":
            let pending = try value(row, "pending").split(separator: ",").map { item -> ManifestEntry in
                let parts = item.split(separator: "=")
                return try ManifestEntry(pageIndex: XCTUnwrap(UInt32(parts[0])), digest: XCTUnwrap(UInt64(parts[1])))
            }
            return .writeBegun(try ServiceWriteReply(read: read(row, size: size), reuseGrant: value(row, "reuse_grant") == "true", pending: pending, reuse: indices(value(row, "reuse"))))
        case "committed": return .committed(try u64(row, "txid"))
        case "commit_status_reply": return .commitStatusReply(try u8(row, "status"))
        case "ownership_reply": return .ownershipReply(try u8(row, "status"))
        case "notice": return try .notice(kind: u8(row, "kind"), connection: u64(row, "connection_id"))
        case "error": return try .error(code: value(row, "sqlstate"), message: value(row, "message"), detail: value(row, "detail"), flags: u8(row, "flags"))
        default: throw JedError("XX000", "unhandled shared frame fixture")
        }
    }
    func testEveryFrozenFrameEncodesAndDecodesByteExactly() throws {
        let rows = try XCTUnwrap(document()["frame"] as? [[String: Any]])
        XCTAssertFalse(rows.isEmpty)
        for row in rows {
            let name = try XCTUnwrap(row["name"] as? String), size = try XCTUnwrap(Int(XCTUnwrap(row["page_size"] as? String)))
            let bytes = try hex(XCTUnwrap(row["hex"] as? String)), request = try XCTUnwrap(UInt32(XCTUnwrap(row["request_id"] as? String)))
            let expected = try ServiceFrame(request: request, message: message(row, size: size))
            XCTAssertEqual(try expected.encode(pageSize: size), bytes, name)
            let decoded = try ServiceFrame.decode(bytes, pageSize: size)
            XCTAssertEqual(decoded.request, request, name)
            XCTAssertEqual(decoded.message.code, expected.message.code, name)
            XCTAssertEqual(try decoded.encode(pageSize: size), bytes, name)
        }
    }
    func testEveryFrozenMalformedFrameIsRejectedBeforeUnboundedAllocation() throws {
        for row in try XCTUnwrap(document()["malformed"] as? [[String: Any]]) {
            let name = try XCTUnwrap(row["name"] as? String), size = try XCTUnwrap(Int(XCTUnwrap(row["page_size"] as? String)))
            let bytes = try hex(XCTUnwrap(row["hex"] as? String))
            XCTAssertThrowsError(try ServiceFrame.decode(bytes, pageSize: size), name) { XCTAssertEqual(($0 as? JedError)?.code, "08P01", name) }
        }
    }
    func testEveryFrozenOwnerRecordEncodesAndDecodesByteExactly() throws {
        for row in try XCTUnwrap(document()["owner_record"] as? [[String: Any]]) {
            let expected = try ServiceOwnerRecord(serving: u8(row, "flags") & 1 != 0, uid: u32(row, "uid"),
                incarnation: hex(value(row, "incarnation")), pid: u32(row, "pid"), endpoint: value(row, "endpoint"))
            let bytes = try hex(XCTUnwrap(row["hex"] as? String))
            XCTAssertEqual(try expected.encode(), bytes)
            XCTAssertEqual(try XCTUnwrap(ServiceOwnerRecord.decode(bytes)).encode(), bytes)
            var damaged = bytes; damaged[damaged.count - 1] ^= 1
            XCTAssertNil(ServiceOwnerRecord.decode(damaged))
        }
    }
}
