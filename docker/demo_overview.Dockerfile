FROM demo-base AS svg-builder

RUN mkdir -p /home/john/foo/bar/baz

COPY tapes/demo_overview.tape .

RUN faketime @1771881894 /home/john/bin/evp demo_overview.tape

FROM scratch
COPY --from=svg-builder /app/*.svg /
