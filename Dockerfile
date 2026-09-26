# syntax=docker/dockerfile:1

# Build stage: toolchain + libdvdcss (built from source — Ubuntu's
# `libdvdcss-dev` is a libdvd-pkg shim that does not produce the library
# during a plain `docker build`).
FROM ubuntu:24.04 AS build
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl build-essential bzip2 \
    && rm -rf /var/lib/apt/lists/*

ARG DVDCSS_VERSION=1.4.3
RUN curl -fsSL \
        "https://download.videolan.org/pub/libdvdcss/${DVDCSS_VERSION}/libdvdcss-${DVDCSS_VERSION}.tar.bz2" \
        -o /tmp/libdvdcss.tar.bz2 \
    && mkdir -p /tmp/libdvdcss \
    && tar -xjf /tmp/libdvdcss.tar.bz2 -C /tmp/libdvdcss --strip-components=1 \
    && cd /tmp/libdvdcss \
    && ./configure --prefix=/usr --libdir=/usr/lib/x86_64-linux-gnu \
    && make -j"$(nproc)" \
    && make install \
    && ldconfig

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /src
COPY . .
RUN cargo build --release --features dvdcss

# Runtime stage: just the binary and the shared library it links.
FROM ubuntu:24.04
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/lib/x86_64-linux-gnu/libdvdcss.so.2* /usr/lib/x86_64-linux-gnu/
COPY --from=build /src/target/release/packrat /usr/local/bin/packrat
ENTRYPOINT ["packrat"]
CMD ["--help"]
