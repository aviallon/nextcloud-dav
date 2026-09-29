# SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Multi-stage build producing a static (musl) binary inside a small alpine
# image. The **image is the deployment unit**: production pulls it from the
# registry and the container runs the binary directly (see the
# `nextcloud-dav` sidecar in the infra repo's
# `helmfile/values/nextcloud-prod.yaml.gotmpl`). The alpine base is kept for
# the wget-based health probes; nothing is copied onto the Nextcloud PVC.
#
#   docker build -t gitea.lesviallon.fr/aviallon/nextcloud-dav:<tag> .
#   docker push gitea.lesviallon.fr/aviallon/nextcloud-dav:<tag>
#   # then bump the tag in the helmfile values and `helmfile apply`
#
# (For a local binary only: docker create + docker cp still works, but that
# path is not how anything is deployed.)

FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev cmake make gcc g++ perl bash linux-headers
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin nextcloud-dav

FROM alpine:3.24
COPY --from=builder /src/target/release/nextcloud-dav /usr/local/bin/nextcloud-dav
ENTRYPOINT ["/usr/local/bin/nextcloud-dav"]
