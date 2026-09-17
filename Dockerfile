# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Multi-stage build producing a static (musl) binary. The binary is what gets
# deployed: like nextcloud/notify_push, it is dropped on the Nextcloud PVC and
# run from an `alpine` sidecar container, so no image has to be distributed to
# the cluster.
#
#   docker build -t nextcloud-dav:build .
#   docker create --name ndav nextcloud-dav:build
#   docker cp ndav:/usr/local/bin/nextcloud-dav ./nextcloud-dav-static
#   docker rm ndav

FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev cmake make gcc g++ perl bash linux-headers
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin nextcloud-dav

FROM alpine:3.24
COPY --from=builder /src/target/release/nextcloud-dav /usr/local/bin/nextcloud-dav
ENTRYPOINT ["/usr/local/bin/nextcloud-dav"]
