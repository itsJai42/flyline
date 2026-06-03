FROM demo-base AS svg-builder

COPY tapes/demo_agent_mode.tape .

RUN faketime @1771881894 /home/john/bin/evp demo_agent_mode.tape

FROM scratch
COPY --from=svg-builder /app/*.svg /
COPY --from=svg-builder /home/john/*log  /
