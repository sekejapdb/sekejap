"use strict";
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));
var __exportStar = (this && this.__exportStar) || function(m, exports) {
    for (var p in m) if (p !== "default" && !Object.prototype.hasOwnProperty.call(exports, p)) __createBinding(exports, m, p);
};
Object.defineProperty(exports, "__esModule", { value: true });
exports.Sekejap = void 0;
// sekejap/orm — the Node entry point. Re-exports the shared, backend-agnostic
// core and provides `Sekejap.open` defaulted to the Node napi backend, so
// backend code needs only `{ schema }`. The @sekejap/react-native package ships
// the same core with a JSI-defaulted `Sekejap` instead.
__exportStar(require("./core"), exports);
const core_1 = require("./core");
const native_1 = require("./native");
exports.Sekejap = {
    /**
     * Open a database. Defaults to the Node napi backend, so backend code passes
     * only `{ schema }`. Pass `{ native }` to override (rarely needed on Node).
     */
    open(path, opts) {
        return (0, core_1.open)(path, { schema: opts.schema, native: opts.native ?? native_1.Native });
    },
};
