FROM demo-base AS svg-builder

COPY tapes/demo_custom_animation.tape .

RUN faketime @1771881894 /home/john/bin/evp demo_custom_animation.tape

FROM scratch
COPY --from=svg-builder /app/*.svg /
