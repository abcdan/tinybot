"""Host runner used only to build and check CPython's WASI interpreter."""
import sys

from wasmtime import Config, Engine, ExitTrap, Linker, Module, Store, WasiConfig

source, command, python_path, binary, *arguments = sys.argv[1:]
wasi = WasiConfig()
wasi.argv = [command, *arguments]
wasi.env = [("PYTHONPATH", python_path)]
wasi.preopen_dir(source, "/", fs_mutable=False)
wasi.inherit_stdout()
wasi.inherit_stderr()
config = Config()
config.parallel_compilation = False
engine = Engine(config)
store = Store(engine)
store.set_wasi(wasi)
linker = Linker(engine)
linker.define_wasi()
try:
    linker.instantiate(store, Module.from_file(engine, binary)).exports(store)["_start"](store)
except ExitTrap as failure:
    sys.exit(failure.code)
