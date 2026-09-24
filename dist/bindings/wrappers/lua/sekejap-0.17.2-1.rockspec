package = "sekejap"
version = "0.17.2-1"
source = {
   -- the sekejap repository and the release tag this rockspec builds
   url = "git+https://github.com/sekejapdb/sekejap.git",
   tag = "v0.17.2",
}
description = {
   summary = "A Lua C module over sekejap's C ABI -- SQL, graph, spatial, vector and full-text in one embedded store.",
   detailed = [[
      sekejap is a graph-first, multi-model embedded database (SQL, graph
      traversal, spatial, vector and full-text) addressed by collection and
      key, with JSON documents and $n parameters. This rock is a thin Lua
      5.4 C module over the stable C ABI (docs/dist/C_ABI.md in the
      sekejap repository): one Lua method per C function, JSON text in and
      out, Lua errors (pcall-catchable) on a hard failure.
   ]],
   homepage = "https://sekejap.life",
   license = "MIT OR Apache-2.0", -- the repository carries both LICENSE-MIT and LICENSE-APACHE
}
dependencies = {
   "lua >= 5.4, < 5.5",
}
build = {
   type = "make",
   -- The Makefile builds against a PREBUILT libsekejap (no Rust here): set
   -- SEKEJAP_PREFIX to the directory holding libsekejap.{dylib,so,a} and
   -- include/sekejap.h if it is not /usr/local (the Makefile's default), e.g.:
   --   luarocks make SEKEJAP_PREFIX=/path/to/libsekejap
   build_variables = {
      CC = "$(CC)",
      CFLAGS = "$(CFLAGS)",
   },
   -- The Makefile has no `install` target (see its header comment); install
   -- the built module the same way the "builtin" build type would.
   install_target = false,
   install = {
      lib = { sekejap = "sekejap.so" },
   },
}
test = {
   type = "command",
   command = "make test",
}
