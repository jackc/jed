extension Session {
    /// Input length is measured in UTF-8 bytes. Zero disables this limit.
    public var maxSQLLength: Int {
        get { settings.options.maxSQLLength }
        set { settings.options.maxSQLLength = max(0, newValue) }
    }
    /// Zero waits without a deadline. This host wait never adds SQL cost.
    public var lockTimeoutMilliseconds: UInt64 {
        get { settings.options.lockTimeoutMilliseconds }
        set { settings.options.lockTimeoutMilliseconds = newValue }
    }
    /// Runtime zero disables operator spilling; an initial option of zero
    /// instead selects the finite default. Existing cursors retain their budget.
    public var workMemoryBytes: Int64 {
        get { settings.options.workMemoryBytes }
        set { settings.options.workMemoryBytes = max(0, newValue) }
    }
    public var defaultPrivileges: PrivilegeSet {
        get { settings.privileges.defaults }
        set { settings.privileges.defaults = newValue; settings.options.defaultPrivileges = newValue }
    }
    public var allowDDL: Bool {
        get { settings.options.allowDDL }
        set { settings.options.allowDDL = newValue }
    }
    public var allowTemporaryDDL: Bool {
        get { settings.options.allowTemporaryDDL ?? settings.options.allowDDL }
        set { settings.options.allowTemporaryDDL = newValue }
    }
    /// The private temporary page-space residency budget; zero never spills.
    /// A changed budget is applied at the next statement's domain preparation.
    public var temporaryBufferBytes: Int64 {
        get { settings.options.temporaryBufferBytes }
        set { settings.options.temporaryBufferBytes = max(0, newValue) }
    }
    /// The private temporary storage ceiling; zero disables this limit.
    public var maxTemporaryStorageBytes: Int64 {
        get { settings.options.maxTemporaryStorageBytes }
        set { settings.options.maxTemporaryStorageBytes = max(0, newValue) }
    }
}
