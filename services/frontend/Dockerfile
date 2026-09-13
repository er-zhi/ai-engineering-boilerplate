# Frontend build: copies the static client bundle into a volume Gateway serves directly. No server runs; the container exits once the copy is done.
FROM alpine:3.24
COPY services/frontend/client/ /dist/
RUN mkdir -p /shared && chown -R 10001:10001 /dist /shared
USER 10001:10001
CMD ["sh", "-c", "cp -a /dist/. /shared/ && echo frontend bundle copied"]
