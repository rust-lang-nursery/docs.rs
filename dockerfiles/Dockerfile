# To produce a smaller image this Dockerfile contains two separate stages: in
# the first one all the build dependencies are installed and docs.rs is built,
# while in the second one just the runtime dependencies are installed, with the
# binary built in the previous stage copied there.
#
# As of 2019-10-29 this reduces the image from 2.8GB to 500 MB.

#################
#  Build stage  #
#################

FROM ubuntu:bionic AS build

# Install packaged dependencies
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential git curl cmake gcc g++ pkg-config libmagic-dev \
    libssl-dev zlib1g-dev ca-certificates

# Install the stable toolchain with rustup
RUN curl https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init >/tmp/rustup-init && \
    chmod +x /tmp/rustup-init && \
    /tmp/rustup-init -y --no-modify-path --default-toolchain stable --profile minimal
ENV PATH=/root/.cargo/bin:$PATH

# Build the dependencies in a separate step to avoid rebuilding all of them
# every time the source code changes. This takes advantage of Docker's layer
# caching, and it works by copying the Cargo.{toml,lock} with dummy source code
# and doing a full build with it.
RUN mkdir -p /build/src/web/badge
WORKDIR /build
COPY benches benches
COPY Cargo.lock Cargo.toml ./
COPY crates/badge crates/badge/
COPY crates/metadata crates/metadata/
COPY crates/font-awesome-as-a-crate crates/font-awesome-as-a-crate
RUN echo "fn main() {}" > src/main.rs && echo "fn main() {}" > build.rs

RUN cargo fetch
RUN cargo build --release

# Dependencies are now cached, copy the actual source code and do another full
# build. The touch on all the .rs files is needed, otherwise cargo assumes the
# source code didn't change thanks to mtime weirdness.
RUN rm -rf src build.rs

COPY .git .git
COPY build.rs build.rs
RUN touch build.rs
COPY src src/
RUN find src -name "*.rs" -exec touch {} \;
COPY templates/style templates/style
COPY vendor vendor/

RUN cargo build --release

##################
#  Output stage  #
##################

FROM ubuntu:bionic AS output

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y \
    git \
    libmagic1 \
    docker.io \
    ca-certificates \
    build-essential \
    gcc \
    pkg-config \
    libssl-dev

RUN mkdir -p /opt/docsrs/prefix

COPY --from=build /build/target/release/docsrs /usr/local/bin
COPY static /opt/docsrs/static
COPY templates /opt/docsrs/templates
COPY dockerfiles/entrypoint.sh /opt/docsrs/
COPY vendor /opt/docsrs/vendor

WORKDIR /opt/docsrs
ENTRYPOINT ["/opt/docsrs/entrypoint.sh"]
CMD ["daemon", "--registry-watcher=disabled"]
