// Rendered demo compatibility facade. The production typed browser API lives in
// async.ts; both use the same bounded transport, scheduler, and worker lifecycle.
import { EngineError } from "../errors.ts";
import { render } from "../value.ts";
import {
  createDatabase,
  openDatabase,
  type AsyncDatabase,
  type AsyncSession,
  type BrowserDatabaseOptions,
  type BrowserOpenOptions,
} from "./async.ts";

export type QueryResult = { columnNames: string[]; rows: string[][]; cost: string };
export type StatementResult = { rowsAffected: number | null; cost: string };

export class OpfsError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = "OpfsError";
    this.code = code;
  }
}

async function legacyError<T>(operation: () => Promise<T>): Promise<T> {
  try {
    return await operation();
  } catch (error) {
    if (error instanceof EngineError) throw new OpfsError(error.code(), error.message);
    throw error;
  }
}

export class OpfsDatabase {
  private readonly database: AsyncDatabase;
  private readonly session: AsyncSession;

  private constructor(database: AsyncDatabase, session: AsyncSession) {
    this.database = database;
    this.session = session;
  }

  static async create(
    name: string,
    options: Omit<BrowserDatabaseOptions, "path"> = {},
  ): Promise<OpfsDatabase> {
    return legacyError(async () => {
      const database = await createDatabase({ ...options, path: name });
      try {
        return new OpfsDatabase(database, await database.session());
      } catch (error) {
        await database.close();
        throw error;
      }
    });
  }

  static async open(name: string, options: BrowserOpenOptions = {}): Promise<OpfsDatabase> {
    return legacyError(async () => {
      const database = await openDatabase(name, options);
      try {
        return new OpfsDatabase(database, await database.session());
      } catch (error) {
        await database.close();
        throw error;
      }
    });
  }

  query(sql: string): Promise<QueryResult> {
    return legacyError(async () => {
      const result = await this.session.query(sql);
      try {
        const rows: string[][] = [];
        for await (const row of result) rows.push(row.map(render));
        if (result.columnNames.length === 0) {
          throw new OpfsError(
            "XX000",
            "query() called on a statement that produces no rows; use execute",
          );
        }
        return { columnNames: result.columnNames, rows, cost: result.cost.toString() };
      } finally {
        await result.close();
      }
    });
  }

  execute(sql: string): Promise<StatementResult> {
    return legacyError(async () => {
      const result = await this.session.query(sql);
      try {
        for await (const _row of result) {
          /* Drain before reading the final cost. */
        }
        if (result.columnNames.length !== 0) {
          throw new OpfsError("XX000", "execute() called on a query; use query");
        }
        return { rowsAffected: result.rowsAffected, cost: result.cost.toString() };
      } finally {
        await result.close();
      }
    });
  }

  commit(): Promise<void> {
    return legacyError(async () => {
      await this.session.execute("COMMIT");
    });
  }

  rollback(): Promise<void> {
    return legacyError(async () => {
      await this.session.execute("ROLLBACK");
    });
  }

  close(): Promise<void> {
    return legacyError(() => this.database.close());
  }
}
