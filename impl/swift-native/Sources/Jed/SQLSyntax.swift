/// Unresolved, handwritten syntax. Names and literal text retain byte identity;
/// no catalog lookup, coercion, execution policy, or persistence lives here.
internal struct SQLSourceSpan: Sendable, Equatable {
    var byteStart: Int
    var byteEnd: Int
    var position: Int
}

internal struct SQLName: Sendable, Equatable {
    var database: SQLText?
    var name: SQLText
    var span: SQLSourceSpan
}

internal struct SQLTypeSyntax: Sendable {
    var name: SQLText
    var modifiers: [String] = []
    var arrayDimensions = 0
    var intervalFields: [String] = []
    var span: SQLSourceSpan
}

internal struct SQLExpression: Sendable {
    var kind: Kind
    var span: SQLSourceSpan
    /// Diagnostic location: the operator for an operator node, otherwise its start.
    var position: Int
    indirect enum Kind: Sendable {
        case number(String), string(SQLText), boolean(Bool), null, parameter(Int)
        case column(qualifier: SQLText?, name: SQLText)
        case currentTimestamp, currentDate
        case unary(String, SQLExpression), binary(String, SQLExpression, SQLExpression)
        case cast(SQLExpression, SQLTypeSyntax), typedString(SQLTypeSyntax, SQLText)
        case collate(SQLExpression, SQLText)
        case field(SQLExpression, SQLText?), subscripted(SQLExpression, [SQLSubscript])
        case row([SQLExpression]), array([SQLExpression]), parenthesized(SQLExpression)
        case scalarQuery(SQLQuery), exists(SQLQuery)
        case isNull(SQLExpression, negated: Bool)
        case isDistinct(SQLExpression, SQLExpression, negated: Bool)
        case isJSON(SQLExpression, kind: String, unique: Bool, negated: Bool)
        case member(SQLExpression, [SQLExpression], negated: Bool)
        case memberQuery(SQLExpression, SQLQuery, negated: Bool)
        case between(SQLExpression, lower: SQLExpression, upper: SQLExpression, negated: Bool)
        case pattern(String, SQLExpression, SQLExpression, escape: SQLExpression?, negated: Bool)
        case quantified(String, SQLExpression, all: Bool, SQLExpression)
        case quantifiedQuery(String, SQLExpression, all: Bool, SQLQuery)
        case conditional(operand: SQLExpression?, arms: [SQLCaseArm], fallback: SQLExpression?)
        case conditionalFunction(String, [SQLExpression]), extract(SQLText, SQLExpression)
        case call(SQLCall)
        case json(SQLJSONExpression)
    }
}

internal struct SQLSubscript: Sendable {
    var lower: SQLExpression?
    var upper: SQLExpression?
    var isSlice: Bool
}
internal struct SQLCaseArm: Sendable { var condition: SQLExpression; var result: SQLExpression; var whenPosition = 0 }
internal struct SQLArgument: Sendable { var name: SQLText?; var value: SQLExpression; var variadic = false }
internal struct SQLCall: Sendable {
    var name: SQLText
    var arguments: [SQLArgument] = []
    var star = false
    var distinct = false
    var orderBy: [SQLOrderTerm] = []
    var withinGroup: [SQLOrderTerm] = []
    var filter: SQLExpression?
    var over: SQLWindow?
    var overName: SQLText?
}
internal struct SQLOrderTerm: Sendable {
    var expression: SQLExpression
    var descending = false
    var nullsFirst: Bool?
    var span: SQLSourceSpan
    /// Index access methods distinguish an explicit ASC from omitted ordering,
    /// and parenthesized expression keys from bare column spelling.
    var directionSpecified = false
    var indexExpression = false
}
internal struct SQLWindow: Sendable {
    var baseName: SQLText?
    var partitionBy: [SQLExpression] = []
    var orderBy: [SQLOrderTerm] = []
    var frame: SQLWindowFrame?
}
internal struct SQLWindowFrame: Sendable {
    var mode: String
    var start: SQLFrameBound
    var end: SQLFrameBound?
    var exclusion: String?
}
internal enum SQLFrameBound: Sendable {
    case unbounded(preceding: Bool), currentRow, offset(SQLExpression, preceding: Bool)
}
internal struct SQLJSONExpression: Sendable {
    var name: String
    var context: SQLExpression
    var path: SQLExpression?
    var formatJSON = false
    var encoding: SQLText?
    var passing: [SQLJSONPassing] = []
    var returning: SQLTypeSyntax?
    var wrapper: String?
    var keepQuotes = true
    var uniqueKeys = false
    var onEmpty: SQLJSONBehavior?
    var onError: SQLJSONBehavior?
}
internal struct SQLJSONPassing: Sendable {
    var value: SQLExpression
    var formatJSON: Bool
    var encoding: SQLText?
    var name: SQLText
}
internal struct SQLJSONBehavior: Sendable { var action: String; var value: SQLExpression? }

internal struct SQLQuery: Sendable {
    var body: Body
    var orderBy: [SQLOrderTerm] = []
    var limit: String?
    var offset: String?
    var span: SQLSourceSpan
    indirect enum Body: Sendable {
        case select(SQLSelect), values([[SQLExpression]])
        case set(String, all: Bool, SQLQuery, SQLQuery)
        case nested(SQLQuery)
        case with(SQLWith, SQLQuery)
    }
}
internal struct SQLSelect: Sendable {
    var distinct = false
    var distinctOn: [SQLExpression] = []
    var items: [SQLSelectItem] = []
    var into: SQLInto?
    var from: [SQLFrom] = []
    var predicate: SQLExpression?
    var grouping: [SQLGroup] = []
    var having: SQLExpression?
    var windows: [SQLNamedWindow] = []
}
internal struct SQLInto: Sendable { var name: SQLName; var temporary: Bool }
internal struct SQLSelectItem: Sendable {
    var expression: SQLExpression?
    var star = false
    var qualifier: SQLText?
    var alias: SQLText?
    var span: SQLSourceSpan
}
internal struct SQLNamedWindow: Sendable { var name: SQLText; var definition: SQLWindow }
internal indirect enum SQLGroup: Sendable {
    case set([SQLExpression]), rollup([SQLGroup]), cube([SQLGroup]), sets([SQLGroup])
}
internal struct SQLFrom: Sendable {
    var source: Source
    var lateral = false
    var alias: SQLText?
    var columnAliases: [SQLText] = []
    var columnDefinitions: [SQLColumnDefinition] = []
    var joins: [SQLJoin] = []
    var span: SQLSourceSpan
    /// Nesting within this SQL text, used when a named view expands in place.
    var parserDepth = 0
    indirect enum Source: Sendable {
        case table(SQLName), query(SQLQuery), function(SQLExpression)
        case joined(SQLFrom), jsonTable(SQLJSONTable)
    }
}
internal struct SQLJoin: Sendable {
    var kind: String
    var natural = false
    var right: SQLFrom
    var condition: SQLExpression?
    var using: [SQLText] = []
}
internal struct SQLJSONTable: Sendable {
    var context: SQLExpression
    var formatJSON = false
    var encoding: SQLText?
    var path: SQLExpression
    var pathName: SQLText?
    var passing: [SQLJSONPassing] = []
    var columns: [SQLJSONTableColumn] = []
    var onError: SQLJSONBehavior?
}
internal indirect enum SQLJSONTableColumn: Sendable {
    case ordinality(SQLText)
    case regular(SQLText, SQLTypeSyntax, formatJSON: Bool, exists: Bool,
                 path: SQLText?, wrapper: String?, keepQuotes: Bool,
                 onEmpty: SQLJSONBehavior?, onError: SQLJSONBehavior?)
    case nested(SQLText, name: SQLText?, [SQLJSONTableColumn])
}
internal struct SQLWith: Sendable { var recursive: Bool; var bindings: [SQLCTE] }
internal struct SQLCTE: Sendable {
    var name: SQLText
    var columns: [SQLText] = []
    var materialized: Bool?
    var body: SQLStatement
    var search: SQLSearch?
    var cycle: SQLCycle?
}
internal struct SQLSearch: Sendable { var depthFirst: Bool; var columns: [SQLText]; var output: SQLText }
internal struct SQLCycle: Sendable {
    var columns: [SQLText]
    var mark: SQLText
    var markValue: SQLExpression?
    var defaultValue: SQLExpression?
    var path: SQLText
}

internal indirect enum SQLStatement: Sendable {
    case query(SQLQuery), with(SQLWith, SQLStatement)
    case createTable(SQLCreateTable), alterTable(SQLAlterTable)
    case createIndex(SQLCreateIndex), createType(SQLCreateType)
    case createSequence(SQLCreateSequence), alterSequence(SQLAlterSequence)
    case createView(SQLCreateView), alterView(SQLAlterView)
    case drop(SQLDrop), reindex(SQLReindex), analyze(SQLName, columns: [SQLText])
    case insert(SQLInsert), update(SQLUpdate), delete(SQLDelete), merge(SQLMerge)
    case explain(SQLExplain, SQLStatement)
    case begin(writable: Bool?), commit, rollback
    case savepoint(SQLText), release(SQLText), rollbackTo(SQLText)
}
internal struct SQLExplain: Sendable { var analyze = false; var verbose = false; var costs = true; var lane = false }
internal struct SQLDrop: Sendable { var object: String; var names: [SQLName]; var ifExists: Bool; var cascade: Bool }
internal struct SQLReindex: Sendable {
    var object: String
    var name: SQLName?
    var concurrently = false
    var verbose = false
    var tablespace: SQLText?
}
internal struct SQLCreateTable: Sendable {
    var name: SQLName
    var temporary = false
    var ifNotExists = false
    var columns: [SQLColumnDefinition] = []
    var constraints: [SQLConstraint] = []
    var onCommit: String?
    var query: SQLQuery?
    var queryColumnNames: [SQLText] = []
    var withData = true
}
internal struct SQLColumnDefinition: Sendable {
    var name: SQLText
    var type: SQLTypeSyntax
    var constraints: [SQLConstraint] = []
    var span: SQLSourceSpan
}
internal struct SQLConstraint: Sendable {
    var name: SQLText?
    var kind: Kind
    var span: SQLSourceSpan
    enum Kind: Sendable {
        case primaryKey([SQLText]), notNull, unique([SQLText]), check(SQLExpression)
        case `default`(SQLExpression), collate(SQLText), identity(SQLIdentity)
        case foreignKey(columns: [SQLText], SQLReferences)
        case exclude(method: SQLText?, elements: [SQLExcludeElement], predicate: SQLExpression?)
    }
}
internal struct SQLReferences: Sendable {
    var table: SQLName
    var columns: [SQLText] = []
    var match = "simple"
    var onDelete = "no action"
    var onUpdate = "no action"
}
internal struct SQLIdentity: Sendable { var always: Bool; var options: [SQLSequenceOption] = [] }
internal struct SQLExcludeElement: Sendable { var expression: SQLExpression; var op: String }
internal struct SQLCreateIndex: Sendable {
    var name: SQLText?
    var table: SQLName
    var unique = false
    var ifNotExists = false
    var method: SQLText?
    var keys: [SQLOrderTerm] = []
    var predicate: SQLExpression?
}
internal struct SQLCreateType: Sendable { var name: SQLName; var fields: [SQLColumnDefinition] }
internal struct SQLSequenceOption: Sendable {
    var name: String
    var number: String?
    var type: SQLTypeSyntax?
    var identifier: SQLText?
    var enabled: Bool?
}
internal struct SQLCreateSequence: Sendable { var name: SQLName; var ifNotExists: Bool; var options: [SQLSequenceOption] }
internal struct SQLAlterSequence: Sendable { var name: SQLName; var ifExists: Bool; var rename: SQLText?; var options: [SQLSequenceOption] }
internal struct SQLAlterTable: Sendable { var name: SQLName; var ifExists: Bool; var actions: [SQLAlterTableAction] }
internal enum SQLAlterTableAction: Sendable {
    case renameTable(SQLText), renameColumn(SQLText, SQLText), renameConstraint(SQLText, SQLText)
    case addColumn(SQLColumnDefinition, ifNotExists: Bool), dropColumn(SQLText, ifExists: Bool, cascade: Bool)
    case addConstraint(SQLConstraint), dropConstraint(SQLText, ifExists: Bool, cascade: Bool)
    case dropPrimaryKey(cascade: Bool)
    case setDefault(SQLText, SQLExpression), dropDefault(SQLText), setNotNull(SQLText, Bool)
    case setType(SQLText, SQLTypeSyntax, using: SQLExpression?)
    case addIdentity(SQLText, SQLIdentity), alterIdentity(SQLText, always: Bool?, [SQLSequenceOption])
    case dropIdentity(SQLText, ifExists: Bool)
}
internal struct SQLViewOption: Sendable { var name: SQLText; var value: SQLText? }
internal struct SQLCreateView: Sendable {
    var name: SQLName
    var replace = false
    var temporary = false
    var recursive = false
    var ifNotExists = false
    var columns: [SQLText] = []
    var options: [SQLViewOption] = []
    var query: SQLQuery
    var checkOption: String?
}
internal struct SQLAlterView: Sendable { var name: SQLName; var ifExists: Bool; var actions: [SQLAlterViewAction] }
internal enum SQLAlterViewAction: Sendable {
    case rename(SQLText), renameColumn(SQLText, SQLText)
    case setDefault(SQLText, SQLExpression), dropDefault(SQLText)
    case setOptions([SQLViewOption]), resetOptions([SQLText])
}
internal struct SQLAssignment: Sendable { var column: SQLText; var value: SQLExpression?; var position = 0 }
internal struct SQLReturning: Sendable {
    var oldAlias: SQLText?
    var newAlias: SQLText?
    var items: [SQLSelectItem]
}
internal struct SQLInsert: Sendable {
    var table: SQLName
    var alias: SQLText?
    var columns: [SQLText] = []
    var overriding: String?
    var source: Source
    var conflict: SQLConflict?
    var returning: SQLReturning?
    var columnPositions: [Int] = []
    func columnPosition(_ ordinal: Int) -> Int {
        columnPositions.indices.contains(ordinal) ? columnPositions[ordinal] : 0
    }
    enum Source: Sendable { case defaultValues, values([[SQLExpression?]]), query(SQLQuery) }
}
internal struct SQLConflict: Sendable {
    var columns: [SQLConflictColumn] = []
    var constraint: SQLText?
    var predicate: SQLExpression?
    var doNothing: Bool
    var assignments: [SQLAssignment] = []
    var updatePredicate: SQLExpression?
}
internal struct SQLConflictColumn: Sendable {
    var name: SQLText
    var collation: SQLText?
    var operatorClass: SQLText?
    var descending: Bool?
    var nullsFirst: Bool?
}
internal struct SQLUpdate: Sendable {
    var table: SQLName
    var assignments: [SQLAssignment]
    var predicate: SQLExpression?
    var returning: SQLReturning?
}
internal struct SQLDelete: Sendable { var table: SQLName; var predicate: SQLExpression?; var returning: SQLReturning? }
internal struct SQLMerge: Sendable {
    var table: SQLName
    var alias: SQLText?
    var source: SQLFrom
    var condition: SQLExpression
    var clauses: [SQLMergeClause]
    var returning: SQLReturning?
}
internal struct SQLMergeClause: Sendable {
    var match: String
    var condition: SQLExpression?
    var action: Action
    enum Action: Sendable {
        case update([SQLAssignment]), delete, nothing
        case insert(columns: [SQLText], overriding: String?, values: [SQLExpression?]?)
    }
}
