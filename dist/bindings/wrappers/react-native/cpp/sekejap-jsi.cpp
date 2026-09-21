// See sekejap-jsi.h. HostObject implementation over the sekejap C ABI.
#include "sekejap-jsi.h"

extern "C" {
#include "sekejap.h" // from wrappers/c/include (add to the module's header path)
}

#include <memory>
#include <string>

using namespace facebook;

namespace sekejap {
namespace {

// Own a C-ABI string result and free it on scope exit.
std::string takeCStr(char *s) {
  if (!s) return std::string();
  std::string out(s);
  sekejap_string_free(s);
  return out;
}

// One open database: wraps SekejapDb* and exposes the RawDb methods.
class DbHostObject : public jsi::HostObject {
public:
  explicit DbHostObject(SekejapDb *db) : db_(db) {}
  ~DbHostObject() override { if (db_) sekejap_close(db_); }

  jsi::Value get(jsi::Runtime &rt, const jsi::PropNameID &name) override {
    auto n = name.utf8(rt);
    if (n == "execute") return fn(rt, "execute", 1, [this](jsi::Runtime &rt, const jsi::Value *a) {
      return jsi::Value((double)sekejap_execute(db_, a[0].asString(rt).utf8(rt).c_str()));
    });
    if (n == "executeParams") return fn(rt, "executeParams", 2, [this](jsi::Runtime &rt, const jsi::Value *a) {
      return jsi::Value((double)sekejap_execute_params(db_, a[0].asString(rt).utf8(rt).c_str(),
                                                       a[1].asString(rt).utf8(rt).c_str()));
    });
    if (n == "query") return fn(rt, "query", 1, [this](jsi::Runtime &rt, const jsi::Value *a) {
      return jsi::String::createFromUtf8(rt, takeCStr(sekejap_query(db_, a[0].asString(rt).utf8(rt).c_str())));
    });
    if (n == "queryParams") return fn(rt, "queryParams", 2, [this](jsi::Runtime &rt, const jsi::Value *a) {
      return jsi::String::createFromUtf8(rt, takeCStr(sekejap_query_params(
          db_, a[0].asString(rt).utf8(rt).c_str(), a[1].asString(rt).utf8(rt).c_str())));
    });
    if (n == "put") return fn(rt, "put", 2, [this](jsi::Runtime &rt, const jsi::Value *a) {
      sekejap_put(db_, a[0].asString(rt).utf8(rt).c_str(), a[1].asString(rt).utf8(rt).c_str());
      return jsi::Value::undefined();
    });
    if (n == "putMany") return fn(rt, "putMany", 1, [this](jsi::Runtime &rt, const jsi::Value *a) {
      return jsi::Value((double)sekejap_put_many(db_, a[0].asString(rt).utf8(rt).c_str()));
    });
    if (n == "get") return fn(rt, "get", 1, [this](jsi::Runtime &rt, const jsi::Value *a) {
      char *r = sekejap_get(db_, a[0].asString(rt).utf8(rt).c_str());
      if (!r) return jsi::Value::null();
      return jsi::Value(jsi::String::createFromUtf8(rt, takeCStr(r)));
    });
    if (n == "remove") return fn(rt, "remove", 1, [this](jsi::Runtime &rt, const jsi::Value *a) {
      sekejap_remove(db_, a[0].asString(rt).utf8(rt).c_str());
      return jsi::Value::undefined();
    });
    if (n == "compact") return fn(rt, "compact", 0, [this](jsi::Runtime &, const jsi::Value *) {
      sekejap_compact(db_);
      return jsi::Value::undefined();
    });
    // watch/unwatch: pending a C-ABI change-feed export (sekejap_subscribe);
    // the reactive .watch() lands on RN once that exists.
    return jsi::Value::undefined();
  }

private:
  SekejapDb *db_;

  template <typename F>
  static jsi::Value fn(jsi::Runtime &rt, const char *name, unsigned argc, F &&body) {
    return jsi::Function::createFromHostFunction(
        rt, jsi::PropNameID::forAscii(rt, name), argc,
        [body = std::forward<F>(body)](jsi::Runtime &rt, const jsi::Value &,
                                       const jsi::Value *args, size_t) -> jsi::Value {
          return body(rt, args);
        });
  }
};

// The top-level `global.SekejapJSI`: `open(path) -> DbHostObject`.
class RootHostObject : public jsi::HostObject {
public:
  jsi::Value get(jsi::Runtime &rt, const jsi::PropNameID &name) override {
    if (name.utf8(rt) != "open") return jsi::Value::undefined();
    return jsi::Function::createFromHostFunction(
        rt, jsi::PropNameID::forAscii(rt, "open"), 1,
        [](jsi::Runtime &rt, const jsi::Value &, const jsi::Value *args, size_t) -> jsi::Value {
          SekejapDb *db = sekejap_open(args[0].asString(rt).utf8(rt).c_str());
          if (!db) throw jsi::JSError(rt, "sekejap: open failed");
          return jsi::Object::createFromHostObject(rt, std::make_shared<DbHostObject>(db));
        });
  }
};

} // namespace

void install(jsi::Runtime &rt) {
  rt.global().setProperty(
      rt, "SekejapJSI",
      jsi::Object::createFromHostObject(rt, std::make_shared<RootHostObject>()));
}

} // namespace sekejap
