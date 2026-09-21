"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.Collection = exports.Query = exports.Col = exports.Filter = exports.entity = exports.index = exports.key = exports.bm25 = exports.vector = exports.geo = exports.json = exports.bool = exports.real = exports.int = exports.text = void 0;
exports.open = open;
const text = () => ({ kind: 'text' });
exports.text = text;
const int = () => ({ kind: 'int' });
exports.int = int;
const real = () => ({ kind: 'real' });
exports.real = real;
const bool = () => ({ kind: 'bool' });
exports.bool = bool;
const json = () => ({ kind: 'json' });
exports.json = json;
const geo = () => ({ kind: 'geo' });
exports.geo = geo;
const vector = (dim) => ({ kind: 'vector', dim });
exports.vector = vector;
const bm25 = (inner) => ({ ...inner, kind: 'bm25' });
exports.bm25 = bm25;
const key = (c) => ({ ...c, isKey: true });
exports.key = key;
const index = (c, kind = 'btree') => ({ ...c, index: kind });
exports.index = index;
const entity = (name, columns) => ({ name, columns });
exports.entity = entity;
// ── filters ─────────────────────────────────────────────────────────────────
class SqlContext {
    constructor() {
        this.params = [];
    }
    ph(v) {
        this.params.push(v);
        return `$${this.params.length}`;
    }
    paramsJson() {
        return JSON.stringify(this.params);
    }
}
class Filter {
    and(o) {
        return new AndFilter(this, o);
    }
    or(o) {
        return new OrFilter(this, o);
    }
}
exports.Filter = Filter;
class Cmp extends Filter {
    constructor(col, op, val) {
        super();
        this.col = col;
        this.op = op;
        this.val = val;
    }
    render(ctx) {
        return `${this.col} ${this.op} ${ctx.ph(this.val)}`;
    }
}
class Between extends Filter {
    constructor(col, lo, hi) {
        super();
        this.col = col;
        this.lo = lo;
        this.hi = hi;
    }
    render(ctx) {
        return `${this.col} BETWEEN ${ctx.ph(this.lo)} AND ${ctx.ph(this.hi)}`;
    }
}
class AndFilter extends Filter {
    constructor(a, b) {
        super();
        this.a = a;
        this.b = b;
    }
    render(ctx) {
        return `(${this.a.render(ctx)} AND ${this.b.render(ctx)})`;
    }
}
class OrFilter extends Filter {
    constructor(a, b) {
        super();
        this.a = a;
        this.b = b;
    }
    render(ctx) {
        return `(${this.a.render(ctx)} OR ${this.b.render(ctx)})`;
    }
}
/** A typed column reference — the `d.field` in `where(d => d.field.eq(x))`. */
class Col {
    constructor(name) {
        this.name = name;
    }
    eq(v) {
        return new Cmp(this.name, '=', v);
    }
    neq(v) {
        return new Cmp(this.name, '!=', v);
    }
    lt(v) {
        return new Cmp(this.name, '<', v);
    }
    lte(v) {
        return new Cmp(this.name, '<=', v);
    }
    gt(v) {
        return new Cmp(this.name, '>', v);
    }
    gte(v) {
        return new Cmp(this.name, '>=', v);
    }
    between(lo, hi) {
        return new Between(this.name, lo, hi);
    }
}
exports.Col = Col;
const metricFn = (m) => (m === 'dot' ? 'VECTOR_DOT' : 'VECTOR_COSINE');
// ── query builder ─────────────────────────────────────────────────────────────
class Query {
    constructor(raw, collection, cols, fromRow) {
        this.raw = raw;
        this.collection = collection;
        this.cols = cols;
        this.fromRow = fromRow;
        this.whereF = null;
        this.extra = [];
        this.order = null;
        this.desc = false;
        this.rankTerms = [];
        this.lim = null;
        this.off = null;
    }
    where(build) {
        const f = build(this.cols);
        this.whereF = this.whereF ? new AndFilter(this.whereF, f) : f;
        return this;
    }
    sortBy(select, desc = false) {
        this.order = select(this.cols).name;
        this.desc = desc;
        return this;
    }
    near(select, p, opts) {
        this.extra.push(`ST_DWithin(${select(this.cols).name}, POINT(${p.lon} ${p.lat}), ${opts.metres})`);
        return this;
    }
    matchText(select, terms) {
        this.extra.push(`BM25(${select(this.cols).name}, ${lit(terms)}) > 0.0`);
        return this;
    }
    rankByText(select, terms, weight = 1.0, normalized = true) {
        const fn = normalized ? 'BM25_NORM' : 'BM25';
        this.rankTerms.push(`${fn}(${select(this.cols).name}, ${lit(terms)}) * ${weight}`);
        return this;
    }
    rankByVector(select, v, metric = 'cosine', weight = 1.0) {
        this.rankTerms.push(`${metricFn(metric)}(${select(this.cols).name}, [${v.join(', ')}]) * ${weight}`);
        return this;
    }
    limit(n) {
        this.lim = n;
        return this;
    }
    offset(n) {
        this.off = n;
        return this;
    }
    // Flatten top-level ANDs (keeps `x >= a AND x <= b` a flat range for the index).
    clauses(ctx) {
        const out = [];
        const flat = (f) => {
            if (f instanceof AndFilter) {
                flat(f.a);
                flat(f.b);
            }
            else
                out.push(f.render(ctx));
        };
        if (this.whereF)
            flat(this.whereF);
        out.push(...this.extra);
        return out;
    }
    build(projection = '*') {
        const ctx = new SqlContext();
        let sql = `SELECT ${projection} FROM ${this.collection}`;
        const cs = this.clauses(ctx);
        if (cs.length)
            sql += ` WHERE ${cs.join(' AND ')}`;
        if (this.rankTerms.length)
            sql += ` ORDER BY ${this.rankTerms.join(' + ')} DESC`;
        else if (this.order)
            sql += ` ORDER BY ${this.order} ${this.desc ? 'DESC' : 'ASC'}`;
        if (this.lim != null)
            sql += ` LIMIT ${this.lim}`;
        if (this.off != null)
            sql += ` OFFSET ${this.off}`;
        return [sql, ctx.paramsJson()];
    }
    find() {
        const [sql, params] = this.build();
        const rows = JSON.parse(this.raw.queryParams(sql, params));
        return rows.map((r) => this.fromRow(r));
    }
    findFirst() {
        const saved = this.lim;
        this.lim = 1;
        const r = this.find();
        this.lim = saved;
        return r[0] ?? null;
    }
    count() {
        const [sql, params] = this.build('COUNT(*) AS n');
        const rows = JSON.parse(this.raw.queryParams(sql, params));
        return rows.length ? Number(rows[0].n) : 0;
    }
    update(assign) {
        const ctx = new SqlContext();
        const sets = Object.entries(assign).map(([k, v]) => `${k} = ${ctx.ph(v)}`).join(', ');
        let sql = `UPDATE ${this.collection} SET ${sets}`;
        const cs = this.clauses(ctx);
        if (cs.length)
            sql += ` WHERE ${cs.join(' AND ')}`;
        return this.raw.executeParams(sql, ctx.paramsJson());
    }
    deleteAll() {
        const ctx = new SqlContext();
        let sql = `DELETE FROM ${this.collection}`;
        const cs = this.clauses(ctx);
        if (cs.length)
            sql += ` WHERE ${cs.join(' AND ')}`;
        return this.raw.executeParams(sql, ctx.paramsJson());
    }
    /** Reactive (callback form): the current list now, then a fresh list after
     *  every commit that touches this collection. Returns an unsubscribe fn. */
    subscribe(onData) {
        onData(this.find());
        const id = this.raw.watch((json) => {
            const ev = JSON.parse(json);
            if (ev.collections.includes(this.collection))
                onData(this.find());
        });
        return () => this.raw.unwatch(id);
    }
    /** Reactive (async-iterable form): `for await (const rows of query.watch())`.
     *  Cleans up the native listener when the loop ends. */
    async *watch() {
        const queue = [];
        let wake = null;
        const unsub = this.subscribe((rows) => {
            queue.push(rows);
            wake?.();
            wake = null;
        });
        try {
            while (true) {
                if (queue.length === 0)
                    await new Promise((r) => (wake = r));
                while (queue.length)
                    yield queue.shift();
            }
        }
        finally {
            unsub();
        }
    }
}
exports.Query = Query;
const lit = (s) => `'${s.replace(/'/g, "''")}'`;
// ── collection ────────────────────────────────────────────────────────────────
class Collection {
    constructor(raw, ent) {
        this.raw = raw;
        this.ent = ent;
        this.fromRow = (p) => {
            const row = {};
            for (const [k, def] of Object.entries(this.ent.columns)) {
                const src = def.isKey ? p['_key'] : p[k];
                if (def.kind === 'geo' && src)
                    row[k] = { lon: src.coordinates[0], lat: src.coordinates[1] };
                else if (def.kind === 'vector')
                    row[k] = Array.isArray(src) ? src : [];
                else
                    row[k] = src;
            }
            return row;
        };
        const refs = {};
        let keyCol = 'id';
        for (const [k, def] of Object.entries(ent.columns)) {
            const column = def.isKey ? '_key' : k;
            if (def.isKey)
                keyCol = k;
            refs[k] = new Col(column);
        }
        this.cols = refs;
        this.keyCol = keyCol;
    }
    toPayload(o) {
        const p = { _collection: this.ent.name };
        for (const [k, def] of Object.entries(this.ent.columns)) {
            const v = o[k];
            const column = def.isKey ? '_key' : k;
            if (def.kind === 'geo' && v)
                p[column] = { type: 'Point', coordinates: [v.lon, v.lat] };
            else
                p[column] = v;
        }
        return p;
    }
    query() {
        return new Query(this.raw, this.ent.name, this.cols, this.fromRow);
    }
    where(build) {
        return this.query().where(build);
    }
    near(select, p, opts) {
        return this.query().near(select, p, opts);
    }
    matchText(select, terms) {
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
    put(o) {
        this.putAll([o]);
    }
    putAll(items) {
        const pairs = items.map((o) => [`${this.ent.name}/${o[this.keyCol]}`, JSON.stringify(this.toPayload(o))]);
        this.raw.putMany(JSON.stringify(pairs));
    }
    get(k) {
        const raw = this.raw.get(`${this.ent.name}/${k}`);
        return raw == null ? null : this.fromRow(JSON.parse(raw));
    }
    delete(k) {
        this.raw.executeParams(`DELETE FROM ${this.ent.name} WHERE _key = $1`, JSON.stringify([k]));
    }
    /** Reactive over the whole collection (async-iterable). For a filtered watch,
     *  use `collection.where(...).watch()`. */
    watch() {
        return this.query().watch();
    }
}
exports.Collection = Collection;
class SekejapBase {
    constructor(raw) {
        this.raw = raw;
    }
    compact() {
        this.raw.compact();
    }
}
function ddl(ent) {
    const sqlType = (k) => k === 'int' ? 'INTEGER' : k === 'real' ? 'REAL' : k === 'bool' ? 'BOOLEAN'
        : k === 'geo' ? 'GEO' : k === 'vector' ? 'VECTOR' : k === 'json' ? 'JSON' : 'TEXT';
    const cols = [];
    const indexes = [];
    for (const [k, def] of Object.entries(ent.columns)) {
        const column = def.isKey ? '_key' : k;
        cols.push(def.isKey ? `${column} ${sqlType(def.kind)} PRIMARY KEY` : `${column} ${sqlType(def.kind)}`);
        const using = def.kind === 'geo' ? 'spatial' : def.kind === 'vector' ? 'hnsw' : def.kind === 'bm25' ? 'bm25' : def.index;
        if (using && !def.isKey)
            indexes.push(`CREATE INDEX ON ${ent.name} USING ${using} (${column})`);
    }
    return { create: `CREATE TABLE ${ent.name} (${cols.join(', ')})`, indexes };
}
/**
 * Open a database with an explicit backend. This core is backend-agnostic, so
 * `native` is required here — platform packages wrap this and inject their
 * default (`sekejap` → napi, `@sekejap/react-native` → JSI) as `Sekejap.open`,
 * so app code needs only `{ schema }`.
 */
function open(path, opts) {
    const raw = opts.native.open(path);
    for (const ent of Object.values(opts.schema)) {
        const { create, indexes } = ddl(ent);
        try {
            raw.execute(create);
        }
        catch {
            /* exists on reopen */
        }
        for (const i of indexes) {
            try {
                raw.execute(i);
            }
            catch {
                /* exists */
            }
        }
    }
    const db = new SekejapBase(raw);
    for (const [accessor, ent] of Object.entries(opts.schema)) {
        db[accessor] = new Collection(raw, ent);
    }
    return db;
}
