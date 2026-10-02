import { randomUUID } from 'node:crypto';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

/**
 * The store for the e2e suite (D38): one directory of SQLite files per
 * worker, created before the daemon starts and deleted when the worker ends.
 * The daemon migrates into it, so workers cannot see each other's events.
 * That matters more here than in the Rust tests: an SSE spec asserts on what
 * a stream did NOT deliver, and a stray event from another worker would look
 * like a filtering bug.
 */
export interface Store {
  /** The directory the daemon is handed as HIVE_SANDBOX_DATA_DIR. */
  dir: string;
  /** The main file inside it, which the daemon names. */
  file: string;
  drop(): Promise<void>;
}

export async function createStore(): Promise<Store> {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-e2e-store-'));
  return {
    dir,
    file: path.join(dir, 'hive.db'),
    drop: async () => {
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

/**
 * A writer that appends events exactly the way any other writer would.
 *
 * Deliberately NOT going through the daemon: the design claim is that the
 * events table is the transport and anything that writes a row is a
 * publisher. Driving the stream from outside the process under test is what
 * makes that claim testable rather than assumed. From another process there
 * is no bell to ring at all (the daemon's is in-process, D38), so every row
 * this writer appends reaches a subscriber by the backstop poll alone, which
 * is the half of invariant 4 that is easy to write down and easy to never
 * test.
 */
export class EventWriter {
  private constructor(
    private readonly db: DatabaseSync,
    private readonly actorID: string,
  ) {}

  static async connect(store: Store): Promise<EventWriter> {
    const db = new DatabaseSync(store.file);
    // The daemon's own settings: WAL is a property of the file and already
    // set; the busy timeout is what makes a write that lands while the
    // daemon holds the lock wait rather than fail.
    db.exec('PRAGMA busy_timeout = 10000');

    // The daemon bootstrapped the root actor; events need an owner and an
    // author, and this is the one the e2e token authenticates as.
    const rows = db.prepare('select id from actors where created_by_actor is null').all() as { id: string }[];
    const root = rows[0];
    if (rows.length !== 1 || root === undefined) {
      throw new Error(`expected exactly one root actor, found ${String(rows.length)}`);
    }
    return new EventWriter(db, root.id);
  }

  /** The root actor, which owns everything this writer appends. */
  get owner(): string {
    return this.actorID;
  }

  /** Appends one event, the same row store::append_events writes. */
  async append(kind: string, body: Record<string, unknown> = {}, owner = this.actorID): Promise<string> {
    const row = this.db
      .prepare(
        `insert into events (kind, owner_kind, owner_id, author_actor, principal_kind, principal_id, body)
         values (?, 'user', ?, ?, 'user', ?, ?)
         returning id`,
      )
      .get(kind, owner, this.actorID, this.actorID, JSON.stringify(body)) as { id: number } | undefined;
    if (row === undefined) {
      throw new Error(`insert of ${kind} returned no row`);
    }
    return String(row.id);
  }

  /**
   * Appends an event and rings nothing.
   *
   * Invariant 4 says the events table is the transport and NOTIFY is only a
   * wakeup bell, so a consumer has to stay correct when every notification is
   * dropped. With the bell in-process (D38) nothing written from here can
   * ring it, so this is the same write as `append`; the name stays because
   * the spec that calls it is the one that states the claim.
   */
  async appendWithoutNotify(kind: string, body: Record<string, unknown> = {}): Promise<string> {
    return this.append(kind, body);
  }

  /**
   * Appends an event owned by somebody else, so a spec can assert on what a
   * stream EXCLUDES. events.owner_id carries no foreign key (the table is
   * append-only and nothing references it), so a bare uuid is enough to
   * stand in for another principal here.
   */
  async appendForeign(kind: string): Promise<string> {
    return this.append(kind, {}, randomUUID());
  }

  async close(): Promise<void> {
    this.db.close();
  }
}
