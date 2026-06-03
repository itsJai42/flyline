FROM demo-base AS svg-builder

COPY tapes/demo_tab_completion_easing.tape .

RUN faketime @1771881894 /home/john/bin/evp demo_tab_completion_easing.tape

FROM scratch
COPY --from=svg-builder /app/*.svg /
