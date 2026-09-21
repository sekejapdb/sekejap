// JSI bridge for sekejap on React Native (New Architecture).
//
// Exposes a synchronous `global.SekejapJSI` HostObject whose `open(path)` returns
// a per-database HostObject implementing the RawDb contract (execute, query,
// put, get, …) directly over the sekejap C ABI (../../c/include/sekejap.h).
// JSI = no bridge serialisation, so `db.dishes.where(...).find()` is sync,
// exactly like the Node backend.
//
// STATUS: scaffold — links against the C ABI; not yet built/validated inside a
// New-Arch RN app. Wiring to complete: iOS podspec + Android CMake + a call to
// `sekejap::jsi::install(runtime)` from the TurboModule/JNI OnLoad.
#pragma once

#include <jsi/jsi.h>

namespace sekejap {

// Install `global.SekejapJSI` into a JS runtime. Call once, from the platform
// module init (iOS: TurboModule installer; Android: JNI_OnLoad / OnLoad).
void install(facebook::jsi::Runtime &rt);

} // namespace sekejap
