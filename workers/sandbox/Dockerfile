ARG WASMTIME_VERSION=51.0.0
FROM python:3.14-bookworm AS build
ARG TARGETARCH
ARG PYTHON_WASI_VERSION=3.14.8
ARG WASMTIME_VERSION
RUN case "$TARGETARCH" in amd64) sdk=x86_64;; arm64) sdk=arm64;; *) exit 1;; esac \
    && mkdir -p /opt/wasi-sdk /src /bundle/lib/python3.14 \
    && curl -fsSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-24/wasi-sdk-24.0-${sdk}-linux.tar.gz" \
        | tar -xz -C /opt/wasi-sdk --strip-components=1 \
    && curl -fsSL "https://codeload.github.com/python/cpython/tar.gz/refs/tags/v${PYTHON_WASI_VERSION}" \
        | tar -xz -C /src --strip-components=1
RUN pip install --no-cache-dir "wasmtime==${WASMTIME_VERSION}"
COPY build_runner.py /opt/tinybot/build_runner.py
WORKDIR /src
ENV TERM=dumb
RUN python Tools/wasm/wasi configure-build-python --quiet -- --config-cache --disable-test-modules \
    && python Tools/wasm/wasi make-build-python --quiet
RUN python Tools/wasm/wasi configure-host --quiet --wasi-sdk /opt/wasi-sdk \
    --host-runner "/usr/local/bin/python /opt/tinybot/build_runner.py {HOST_DIR} {ARGV0} {ENV_VAR_VALUE}" \
    -- --config-cache --disable-test-modules \
    && python Tools/wasm/wasi make-host --quiet \
    && cp cross-build/wasm32-wasip1/python.wasm /bundle/python.wasm \
    && cp -a Lib/. /bundle/lib/python3.14/ \
    && cp cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14/_sysconfigdata*.py /bundle/lib/python3.14/
COPY wasm_runtime.py /opt/tinybot/wasm_runtime.py
RUN python /opt/tinybot/wasm_runtime.py /bundle/python.wasm /bundle/python.cwasm \
    && rm /bundle/python.wasm

FROM python:3.14-slim-bookworm
ARG WASMTIME_VERSION
ARG TINYBOT_RELEASE_SHA
RUN apt-get update && apt-get install -y --no-install-recommends fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir "wasmtime==${WASMTIME_VERSION}" \
        "reportlab==5.1.2" "python-docx==1.2.0" "python-pptx==1.0.2" "openpyxl==3.1.5" \
    && mkdir -p /sandbox /work \
    && chown 1000:1000 /sandbox /work
COPY --from=build /bundle /opt/python-wasi
COPY server.py wasm_runtime.py documents.py office.py pdf_document.py /opt/tinybot/
ENV PYTHONDONTWRITEBYTECODE=1 TINYBOT_RELEASE_SHA=${TINYBOT_RELEASE_SHA}
USER 1000:1000
RUN python /opt/tinybot/server.py --self-test
CMD ["python", "/opt/tinybot/server.py"]
