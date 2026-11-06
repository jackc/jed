/// One host-loaded named zone or alias. UTC and fixed offsets are built in and
/// are not part of this inventory.
public struct TimeZoneInfo: Sendable, Equatable {
    public let name: SQLText
    public let tzdataVersion: SQLText
}

extension Environment {
    public func loadedTimeZones() -> [TimeZoneInfo] {
        timezones.zones.sorted { $0.name.utf8.lexicographicallyPrecedes($1.name.utf8) }.map {
            // TimeZoneDatabase.zones contains only validated JTZ entries.
            TimeZoneInfo(name: $0.name, tzdataVersion: $0.dataVersion!)
        }
    }
}
extension Database {
    public func loadedTimeZones() -> [TimeZoneInfo] { environment.loadedTimeZones() }
}
extension Session {
    public func loadedTimeZones() -> [TimeZoneInfo] { settings.environment.loadedTimeZones() }
}
