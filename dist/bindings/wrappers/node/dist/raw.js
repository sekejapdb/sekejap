"use strict";
// The platform-agnostic native contract the typed layer lowers to. A backend
// (napi on Node, JSI on React Native) implements this; the ergonomic layer
// never imports a specific backend, so the same TS runs on both.
Object.defineProperty(exports, "__esModule", { value: true });
