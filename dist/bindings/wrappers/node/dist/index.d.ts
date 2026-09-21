export * from './core';
import { type Schema, type Db } from './core';
import type { RawDbCtor } from './raw';
export declare const Sekejap: {
    /**
     * Open a database. Defaults to the Node napi backend, so backend code passes
     * only `{ schema }`. Pass `{ native }` to override (rarely needed on Node).
     */
    open<S extends Schema>(path: string, opts: {
        schema: S;
        native?: RawDbCtor;
    }): Db<S>;
};
