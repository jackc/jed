import Foundation
import Jed

enum ExampleFailure: Error {
    case unexpectedResult(String)
}

func memoryExample() throws {
    let database = try Database.create()
    defer { try? database.close() }
    let session = try database.session()
    defer { session.close() }
    try session.execute("CREATE TABLE notes(id i32 PRIMARY KEY, body text NOT NULL)")
    let insert = try session.prepare("INSERT INTO notes VALUES($1,$2)")
    try session.update { transaction in
        try transaction.executePrepared(insert, arguments: [Int32(1), "first note"])
        try transaction.executePrepared(insert, arguments: [Int32(2), "bound, not interpolated"])
    }

    let rows = try session.query("SELECT id,body FROM notes WHERE id >= $1 ORDER BY id", arguments: [Int32(1)])
    defer { rows.close() } // Also releases the snapshot if the loop exits early.
    var count = 0
    while let row = try rows.nextRow() {
        let id: Int32 = try row.get("id")
        let body: String = try row.get("body")
        print("memory: \(id): \(body)")
        count += 1
    }
    guard count == 2 else { throw ExampleFailure.unexpectedResult("memory row count") }
    print("memory: final query cost \(rows.cost)")
}

func durableExample() throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-example-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: directory) }
    let path = directory.appendingPathComponent("notes.jed").path
    do {
        // Defaults use durable commits and native automatic file coordination.
        let database = try Database.create(options: CreateOptions(path: path))
        defer { try? database.close() }
        try database.execute("CREATE TABLE notes(id i32 PRIMARY KEY, body text NOT NULL)")
        try database.update { transaction in
            try transaction.execute("INSERT INTO notes VALUES($1,$2)", arguments: [Int32(7), "survives reopen"])
        }
        try database.close()
    }
    let reopened = try Database.open(path)
    defer { try? reopened.close() }
    guard let row = try reopened.queryRow("SELECT body FROM notes WHERE id=$1", arguments: [Int32(7)]),
          try row.get("body", as: String.self) == "survives reopen" else {
        throw ExampleFailure.unexpectedResult("durable reopen")
    }
    print("durable: committed row survives close and reopen")
    try reopened.close()
}

func cancellationExample() throws {
    let database = try Database.create()
    defer { try? database.close() }
    let session = try database.session()
    defer { session.close() }
    try session.execute("CREATE TABLE numbers(id i32 PRIMARY KEY)")
    try session.execute("INSERT INTO numbers VALUES(1),(2),(3)")
    let cancellation = CancellationToken()
    let rows = try session.query("SELECT id FROM numbers ORDER BY id", cancellation: cancellation)
    defer { rows.close() }
    guard let first = try rows.nextRow(), try first.get(0, as: Int32.self) == 1 else {
        throw ExampleFailure.unexpectedResult("first cursor row")
    }
    // The token is thread-safe; another thread may request cancellation too.
    cancellation.cancel()
    do {
        _ = try rows.nextRow()
        throw ExampleFailure.unexpectedResult("cancelled cursor continued")
    } catch let error as JedError where error.code == "57014" {
        print("cancellation: next pull reports \(error.code)")
    }
    guard let row = try session.queryRow("SELECT count(*) FROM numbers"),
          try row.get(0, as: Int64.self) == 3 else {
        throw ExampleFailure.unexpectedResult("session after cancellation")
    }
    print("cancellation: session remains usable")
}

let selection = CommandLine.arguments.dropFirst()
guard selection.count <= 1 else { throw ExampleFailure.unexpectedResult("expected at most one example name") }
switch selection.first ?? "all" {
case "all": try memoryExample(); try durableExample(); try cancellationExample()
case "memory": try memoryExample()
case "durable": try durableExample()
case "cancellation": try cancellationExample()
default: throw ExampleFailure.unexpectedResult("choose all, memory, durable, or cancellation")
}
