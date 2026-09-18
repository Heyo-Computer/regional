# The search database, pinned and pre-configured.
#
#   docker build -f meilisearch/Dockerfile -t regional-meilisearch .
#
# MEILI_MASTER_KEY is deliberately NOT baked in — inject it at runtime from
# HeyoSecret (or the compose environment) so the image carries no secret.

FROM getmeili/meilisearch:v1.53.2

COPY meilisearch/config.toml /etc/meilisearch/config.toml

ENV MEILI_CONFIG_FILE_PATH=/etc/meilisearch/config.toml \
    MEILI_ENV=production \
    MEILI_DB_PATH=/data.ms \
    MEILI_HTTP_ADDR=0.0.0.0:7700 \
    MEILI_NO_ANALYTICS=true

VOLUME ["/data.ms"]
EXPOSE 7700

HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=5 \
  CMD curl -fsS http://127.0.0.1:7700/health || exit 1
