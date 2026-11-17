# syntax=docker/dockerfile:1

FROM rust:1.100.0-alpine3.24@sha256:de1e0058543a81ed7b046629167db587c67a94bdf37cce14eb27f0ec83baaffb AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Thin LTO and more codegen units build much faster. The binary is a bit larger, which only
# matters for the Remote build.
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
# Build the dependencies first. This layer is cached until Cargo.toml, Cargo.lock or build.rs change.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release --locked
COPY . .
# The image has no git data: the release build gives the version and the date. The arguments
# come after the dependency layer, so a new value does not rebuild the dependencies.
ARG UC_VERSION
ARG UC_RELEASE_DATE
# The copied files can be older than the build output of the step above.
RUN touch src/main.rs && \
    cargo build --release --locked && \
    cp target/release/uc-intg-appletv /uc-intg-appletv
RUN mkdir /data

FROM scratch
USER 65532:65532
COPY --from=build /uc-intg-appletv /uc-intg-appletv
COPY --from=build --chown=65532:65532 /data /data
ENV UC_CONFIG_HOME=/data
VOLUME /data
EXPOSE 9090
ENTRYPOINT ["/uc-intg-appletv"]
