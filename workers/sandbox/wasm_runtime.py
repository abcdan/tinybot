"""Build and run the image's trusted CPython WASI module."""
import platform
import resource
import sys
from pathlib import Path

from wasmtime import Config, Engine, ExitTrap, Linker, Module, Store, Trap, WasiConfig, WasmtimeError

BUNDLE = Path("/opt/python-wasi")
JOB_FILE = ".tinybot-job.py"
FILE_LIMIT = 32_768


def engine():
    config = Config()
    config.parallel_compilation = False
    config.consume_fuel = True
    # Make the compiled layer portable between CPUs of the image's architecture.
    config.target = f"{platform.machine()}-unknown-linux-gnu"
    return Engine(config)


def guest(workspace, diagnostics=False):
    try:
        runtime = engine()
        store = Store(runtime)
        store.set_fuel(900_000_000)
        store.set_limits(memory_size=96 * 1024 * 1024, instances=1, memories=1)

        wasi = WasiConfig()
        wasi.argv = ["python", "-S", "-B", f"/{JOB_FILE}"]
        wasi.env = [("PYTHONHOME", "/stdlib"), ("PYTHONUTF8", "1")]
        wasi.preopen_dir(workspace, "/")
        wasi.preopen_dir(str(BUNDLE), "/stdlib", fs_mutable=False)
        wasi.inherit_stdout()
        wasi.inherit_stderr()
        store.set_wasi(wasi)

        linker = Linker(runtime)
        linker.define_wasi()
        # Deserialize only the artifact compiled into the read-only image.
        module = Module.deserialize_file(runtime, str(BUNDLE / "python.cwasm"))
        instance = linker.instantiate(store, module)
        resource.setrlimit(resource.RLIMIT_FSIZE, (FILE_LIMIT, FILE_LIMIT))
        instance.exports(store)["_start"](store)
    except ExitTrap as failure:
        return failure.code
    except (Trap, WasmtimeError) as failure:
        detail = str(failure) if diagnostics else "Python stopped because the sandbox hit a resource limit or a WASI error."
        print(detail, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    source, output = sys.argv[1:]
    Path(output).write_bytes(Module.from_file(engine(), source).serialize())
