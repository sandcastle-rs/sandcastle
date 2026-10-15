# Stages by index: 0 base, 1 builder, 2 sibling, 3 broken, 4 assets, 5 late, 6 final.
FROM mirror.gcr.io/library/alpine:3.20 AS base
ENV FROM_BASE=1
WORKDIR /base

FROM base AS builder
WORKDIR /src
RUN printf 'built\n' > out.txt && chown 1000:1000 out.txt && echo builder-only > /junk
ENV FROM_BUILDER=1

FROM base AS sibling
ENV SIBLING=1

# Nothing the default target needs: never pulled, never run.
FROM mirror.gcr.io/library/does-not-exist:never AS broken
RUN false

# Copies from a stage defined after it.
FROM scratch AS assets
COPY --from=late /late.txt /

FROM mirror.gcr.io/library/busybox:1.36 AS late
RUN echo late > /late.txt

FROM scratch
WORKDIR /app
# Source paths start at the source's root, not its WORKDIR; the relative
# destination uses this stage's WORKDIR.
COPY --from=builder src/out.txt ./
COPY --from=builder /src/out.txt /same.txt
COPY --from=1 /src/out.txt /same.txt
# The default busybox tag links glibc dynamically; the musl one is static.
COPY --from=mirror.gcr.io/library/busybox:1.36-musl /bin/busybox /bin/busybox
COPY --from=assets /late.txt /late.txt
COPY hello.txt /hello.txt
ENTRYPOINT ["/bin/busybox"]
CMD ["cat", "/app/out.txt"]
