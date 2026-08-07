// sekejap — typed, reactive TypeScript API (schema-as-code, no build step).
// Works on Node (backend) and React Native (device) over the same native core.
import { Native, RawDb } from './native';

// ── schema-as-code ────────────────────────────────────────────────────────────

export interface GeoPoint {
  lon: number;
  lat: number;
}

type ColKind = 'text' | 'int' | 'real' | 'bool' | 'geo' | 'vector' | 'bm25' | 'json';

export interface ColumnDef<T> {
  readonly kind: ColKind;
  readonly _t?: T; // phantom carrier for the TS type
  readonly isKey?: boolean;
  readonly index?: 'btree' | 'hash';
  readonly dim?: number;
}

export const text = (): ColumnDef<string> => ({ kind: 'text' });
export const int = (): ColumnDef<number> => ({ kind: 'int' });
export const real = (): ColumnDef<number> => ({ kind: 'real' });
export const bool = (): ColumnDef<boolean> => ({ kind: 'bool' });
export const json = <T = unknown>(): ColumnDef<T> => ({ kind: 'json' });
export const geo = (): ColumnDef<GeoPoint> => ({ kind: 'geo' });
export const vector = (dim: number): ColumnDef<number[]> => ({ kind: 'vector', dim });
export const bm25 = (inner: ColumnDef<string>): ColumnDef<string> => ({ ...inner, kind: 'bm25' });
export const key = <T>(c: ColumnDef<T>): ColumnDef<T> => ({ ...c, isKey: true });
export const index = <T>(c: ColumnDef<T>, kind: 'btree' | 'hash' = 'btree'): ColumnDef<T> =>
  ({ ...c, index: kind });

export type Columns = Record<string, ColumnDef<any>>;

export interface Entity<C extends Columns> {
  readonly name: string;
  readonly columns: C;
}

export const entity = <C extends Columns>(name: string, columns: C): Entity<C> => ({ name, columns });

/** The row type inferred from an entity's columns. */
export type InferRow<E> = E extends Entity<infer C>
  ? { [K in keyof C]: C[K] extends ColumnDef<infer T> ? T : never }
  : never;

// ── filters ─────────────────────────────────────────────────────────────────

class SqlContext {
  readonly params: unknown[] = [];
  ph(v: unknown): string {
    this.params.push(v);
    return `$${this.params.length}`;
  }
  paramsJson(): string {
    return JSON.stringify(this.params);
  }
}

export abstract class Filter {
  abstract render(ctx: SqlContext): string;
  and(o: Filter): Filter {
    return new AndFilter(this, o);
  }
  or(o: Filter): Filter {
    return new OrFilter(this, o);
  }
}
class Cmp extends Filter {
  constructor(private col: string, private op: string, private val: unknown) {
    super();
  }
  render(ctx: SqlContext) {
    return `${this.col} ${this.op} ${ctx.ph(this.val)}`;
  }
}
class Between extends Filter {
  constructor(private col: string, private lo: unknown, private hi: unknown) {
    super();
  }
  render(ctx: SqlContext) {
    return `${this.col} BETWEEN ${ctx.ph(this.lo)} AND ${ctx.ph(this.hi)}`;
  }
}
class AndFilter extends Filter {
  constructor(readonly a: Filter, readonly b: Filter) {
    super();
  }
  render(ctx: SqlContext) {
    return `(${this.a.render(ctx)} AND ${this.b.render(ctx)})`;
  }
}
class OrFilter extends Filter {
  constructor(readonly a: Filter, readonly b: Filter) {
    super();
  }
  render(ctx: SqlContext) {
    return `(${this.a.render(ctx)} OR ${this.b.render(ctx)})`;
  }
}

/** A typed column reference — the `d.field` in `where(d => d.field.eq(x))`. */
export class Col<T> {
  constructor(readonly name: string) {}
  eq(v: T): Filter {
    return new Cmp(this.name, '=', v);
  }
  neq(v: T): Filter {
    return new Cmp(this.name, '!=', v);
  }
  lt(v: T): Filter {
    return new Cmp(this.name, '<', v);
  }
  lte(v: T): Filter {
    return new Cmp(this.name, '<=', v);
  }
  gt(v: T): Filter {
    return new Cmp(this.name, '>', v);
  }
  gte(v: T): Filter {
    return new Cmp(this.name, '>=', v);
  }
  between(lo: T, hi: T): Filter {
    return new Between(this.name, lo, hi);
  }
}

export type ColRefs<C extends Columns> = {
  [K in keyof C]: C[K] extends ColumnDef<infer T> ? Col<T> : never;
};

export type VectorMetric = 'cosine' | 'dot';
const metricFn = (m: VectorMetric) => (m === 'dot' ? 'VECTOR_DOT' : 'VECTOR_COSINE');

// ── query builder ─────────────────────────────────────────────────────────────

export class Query<Row, C extends Columns> {
  private whereF: Filter | null = null;
  private extra: string[] = [];
  private order: string | null = null;
  private desc = false;
  private rankTerms: string[] = [];
  private lim: number | null = null;
  private off: number | null = null;

  constructor(
    private raw: RawDb,
    private collection: string,
    private cols: ColRefs<C>,
    private fromRow: (payload: any) => Row,
  ) {}

  where(build: (c: ColRefs<C>) => Filter): this {
    const f = build(this.cols);
    this.whereF = this.whereF ? new AndFilter(this.whereF, f) : f;
    return this;
  }
  sortBy(select: (c: ColRefs<C>) => Col<any>, desc = false): this {
    this.order = select(this.cols).name;
    this.desc = desc;
    return this;
  }
  near(select: (c: ColRefs<C>) => Col<any>, p: GeoPoint, opts: { metres: number }): this {
    this.extra.push(`ST_DWithin(${select(this.cols).name}, POINT(${p.lon} ${p.lat}), ${opts.metres})`);
    return this;
  }
  matchText(select: (c: ColRefs<C>) => Col<string>, terms: string): this {
    this.extra.push(`BM25(${select(this.cols).name}, ${lit(terms)}) > 0.0`);
    return this;
  }
  rankByText(select: (c: ColRefs<C>) => Col<string>, terms: string, weight = 1.0, normalized = true): this {
    const fn = normalized ? 'BM25_NORM' : 'BM25';
    this.rankTerms.push(`${fn}(${select(this.cols).name}, ${lit(terms)}) * ${weight}`);
    return this;
  }
  rankByVector(select: (c: ColRefs<C>) => Col<number[]>, v: number[], metric: VectorMetric = 'cosine', weight = 1.0): this {
    this.rankTerms.push(`${metricFn(metric)}(${select(this.cols).name}, [${v.join(', ')}]) * ${weight}`);
    return this;
  }
  limit(n: number): this {
    this.lim = n;
    return this;
  }
  offset(n: number): this {
    this.off = n;
    return this;
  }

  // Flatten top-level ANDs (keeps `x >= a AND x <= b` a flat range for the index).
  private clauses(ctx: SqlContext): string[] {
    const out: string[] = [];
    const flat = (f: Filter) => {
      if (f instanceof AndFilter) {
        flat(f.a);
        flat(f.b);
      } else out.push(f.render(ctx));
    };
    if (this.whereF) flat(this.whereF);
    out.push(...this.extra);
    return out;
  }

  private build(projection = '*'): [string, string] {
    const ctx = new SqlContext();
    let sql = `SELECT ${projection} FROM ${this.collection}`;
    const cs = this.clauses(ctx);
    if (cs.length) sql += ` WHERE ${cs.join(' AND ')}`;
    if (this.rankTerms.length) sql += ` ORDER BY ${this.rankTerms.join(' + ')} DESC`;
    else if (this.order) sql += ` ORDER BY ${this.order} ${this.desc ? 'DESC' : 'ASC'}`;
    if (this.lim != null) sql += ` LIMIT ${this.lim}`;
    if (this.off != null) sql += ` OFFSET ${this.off}`;
    return [sql, ctx.paramsJson()];
  }

  find(): Row[] {
    const [sql, params] = this.build();
    const rows = JSON.parse(this.raw.queryParams(sql, params)) as any[];
    return rows.map((r) => this.fromRow(r));
  }
  findFirst(): Row | null {
    const saved = this.lim;
    this.lim = 1;
    const r = this.find();
    this.lim = saved;
    return r[0] ?? null;
  }
  count(): number {
    const [sql, params] = this.build('COUNT(*) AS n');
    const rows = JSON.parse(this.raw.queryParams(sql, params)) as any[];
    return rows.length ? Number(rows[0].n) : 0;
  }
  update(assign: Partial<Row>): number {
    const ctx = new SqlContext();
    const sets = Object.entries(assign).map(([k, v]) => `${k} = ${ctx.ph(v)}`).join(', ');
    let sql = `UPDATE ${this.collection} SET ${sets}`;
    const cs = this.clauses(ctx);
    if (cs.length) sql += ` WHERE ${cs.join(' AND ')}`;
    return this.raw.executeParams(sql, ctx.paramsJson());
  }
  deleteAll(): number {
    const ctx = new SqlContext();
    let sql = `DELETE FROM ${this.collection}`;
    const cs = this.clauses(ctx);
    if (cs.length) sql += ` WHERE ${cs.join(' AND ')}`;
    return this.raw.executeParams(sql, ctx.paramsJson());
  }

  /** Reactive (callback form): the current list now, then a fresh list after
   *  every commit that touches this collection. Returns an unsubscribe fn. */
  subscribe(onData: (rows: Row[]) => void): () => void {
    onData(this.find());
    const id = this.raw.watch((json: string) => {
      const ev = JSON.parse(json) as { collections: string[] };
      if (ev.collections.includes(this.collection)) onData(this.find());
    });
    return () => this.raw.unwatch(id);
  }

  /** Reactive (async-iterable form): `for await (const rows of query.watch())`.
   *  Cleans up the native listener when the loop ends. */
  async *watch(): AsyncGenerator<Row[]> {
    const queue: Row[][] = [];
    let wake: (() => void) | null = null;
    const unsub = this.subscribe((rows) => {
      queue.push(rows);
      wake?.();
      wake = null;
    });
    try {
      while (true) {
        if (queue.length === 0) await new Promise<void>((r) => (wake = r));
        while (queue.length) yield queue.shift()!;
      }
    } finally {
      unsub();
    }
  }
}

const lit = (s: string) => `'${s.replace(/'/g, "''")}'`;

// ── collection ────────────────────────────────────────────────────────────────

export class Collection<Row extends Record<string, any>, C extends Columns> {
  private cols: ColRefs<C>;
  private keyCol: string;

  constructor(private raw: RawDb, private ent: Entity<C>) {
    const refs: any = {};
    let keyCol = 'id';
    for (const [k, def] of Object.entries(ent.columns)) {
      const column = def.isKey ? '_key' : k;
      if (def.isKey) keyCol = k;
      refs[k] = new Col(column);
    }
    this.cols = refs;
    this.keyCol = keyCol;
  }

  private fromRow = (p: any): Row => {
    const row: any = {};
    for (const [k, def] of Object.entries(this.ent.columns)) {
      const src = def.isKey ? p['_key'] : p[k];
      if (def.kind === 'geo' && src) row[k] = { lon: src.coordinates[0], lat: src.coordinates[1] };
      else if (def.kind === 'vector') row[k] = Array.isArray(src) ? src : [];
      else row[k] = src;
    }
    return row as Row;
  };

  private toPayload(o: Row): any {
    const p: any = { _collection: this.ent.name };
    for (const [k, def] of Object.entries(this.ent.columns)) {
      const v = o[k];
      const column = def.isKey ? '_key' : k;
      if (def.kind === 'geo' && v) p[column] = { type: 'Point', coordinates: [v.lon, v.lat] };
      else p[column] = v;
    }
    return p;
  }

  query(): Query<Row, C> {
    return new Query(this.raw, this.ent.name, this.cols, this.fromRow);
  }
  where(build: (c: ColRefs<C>) => Filter) {
    return this.query().where(build);
  }
  near(select: (c: ColRefs<C>) => Col<any>, p: GeoPoint, opts: { metres: number }) {
    return this.query().near(select, p, opts);
  }
  matchText(select: (c: ColRefs<C>) => Col<string>, terms: string) {
    return this.query().matchText(select, terms);
  }
  all() {
    return this.query();
  }
  find() {
    return this.query().find();
  }
  count() {
    return this.query().count();
  }

  put(o: Row): void {
    this.putAll([o]);
  }
  putAll(items: Row[]): void {
    const pairs = items.map((o) => [`${this.ent.name}/${o[this.keyCol]}`, JSON.stringify(this.toPayload(o))]);
    this.raw.putMany(JSON.stringify(pairs));
  }
  get(k: string): Row | null {
    const raw = this.raw.get(`${this.ent.name}/${k}`);
    return raw == null ? null : this.fromRow(JSON.parse(raw));
  }
  delete(k: string): void {
    this.raw.executeParams(`DELETE FROM ${this.ent.name} WHERE _key = $1`, JSON.stringify([k]));
  }

  /** Reactive over the whole collection (async-iterable). For a filtered watch,
   *  use `collection.where(...).watch()`. */
  watch() {
    return this.query().watch();
  }
}

// ── database ──────────────────────────────────────────────────────────────────

export type Schema = Record<string, Entity<any>>;
export type Db<S extends Schema> = SekejapBase & {
  [K in keyof S]: Collection<InferRow<S[K]>, S[K] extends Entity<infer C> ? C : never>;
};

class SekejapBase {
  constructor(readonly raw: RawDb) {}
  compact(): void {
    this.raw.compact();
  }
}

function ddl(ent: Entity<any>): { create: string; indexes: string[] } {
  const sqlType = (k: ColKind) =>
    k === 'int' ? 'INTEGER' : k === 'real' ? 'REAL' : k === 'bool' ? 'BOOLEAN'
      : k === 'geo' ? 'GEO' : k === 'vector' ? 'VECTOR' : k === 'json' ? 'JSON' : 'TEXT';
  const cols: string[] = [];
  const indexes: string[] = [];
  for (const [k, def] of Object.entries(ent.columns) as [string, ColumnDef<any>][]) {
    const column = def.isKey ? '_key' : k;
    cols.push(def.isKey ? `${column} ${sqlType(def.kind)} PRIMARY KEY` : `${column} ${sqlType(def.kind)}`);
    const using =
      def.kind === 'geo' ? 'spatial' : def.kind === 'vector' ? 'hnsw' : def.kind === 'bm25' ? 'bm25' : def.index;
    if (using && !def.isKey) indexes.push(`CREATE INDEX ON ${ent.name} USING ${using} (${column})`);
  }
  return { create: `CREATE TABLE ${ent.name} (${cols.join(', ')})`, indexes };
}

export const Sekejap = {
  open<S extends Schema>(path: string, opts: { schema: S }): Db<S> {
    const raw = Native.open(path);
    for (const ent of Object.values(opts.schema)) {
      const { create, indexes } = ddl(ent);
      try {
        raw.execute(create);
      } catch {
        /* exists on reopen */
      }
      for (const i of indexes) {
        try {
          raw.execute(i);
        } catch {
          /* exists */
        }
      }
    }
    const db: any = new SekejapBase(raw);
    for (const [accessor, ent] of Object.entries(opts.schema)) {
      db[accessor] = new Collection(raw, ent);
    }
    return db as Db<S>;
  },
};
