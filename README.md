# regional

A region-scoped search stack. Pick a region — a US state, say — and every
document in the index is a thing physically located inside it. Ask for
"restaurants in Denver" or "a vineyard in Grand Junction" and the location
part is answered with real coordinates, not a string match on a place name.

Three services, three VM images:

| | what it is | port |
|---|---|---|
| **`mcp/`** | a remote MCP server exposing read-only search tools | 8080 |
| **`bot/`** | a continuous indexer that keeps the index fresh and growing | 8081 |
| **`dashboard/`** | operator views (8090) and a public request page (8091) | 8090/8091 |
| **`meilisearch/`** | the search database, pinned and pre-configured | 7700 |
| `core/` | the shared schema every binary depends on — not a service | — |

One TOML file is the only thing that makes this Colorado-specific.
`region.colorado.toml` holds the bounding box, the gazetteer and the search
vocabulary; `region.vermont.toml` is a worked second example. See
[Configuring a region](#configuring-a-region).

## Quick start

```bash
make env                 # writes .env, generating the two secrets for you
$EDITOR .env             # CONTACT_EMAIL must be a real address you monitor
make up                  # build and start the whole stack
```

Then:

```bash
make urls                # where every surface is listening
make health              # probe all five health endpoints
make logs-bot            # follow the indexer

open http://localhost:8090                        # operator dashboard
open http://localhost:8091                        # public request page
```

`make` on its own lists every target. Everything it runs is an ordinary
`docker compose` or `cargo` command — `make -n <target>` prints it if you
would rather run it by hand.

Point any MCP client at `http://localhost:8080/mcp` with the header
`Authorization: Bearer $MCP_AUTH_TOKEN`, or use the Inspector:

```bash
npx @modelcontextprotocol/inspector
```

## The tools

| tool | what it does |
|---|---|
| `search_region` | The main entry point. Subject in `query`, location in `city`/`near`. Federated across all three indexes into one ranked list. |
| `find_nearby` | Strictly distance-ordered results around a point, each with `distance_m`. |
| `search_events` | Events in a time window, soonest first. Defaults to upcoming only. |
| `get_document` | One full record, including the body that search results truncate. |
| `describe_region` | The region's bounds, every place name that resolves, live category values, and per-index document counts. Call it first. |

`city` and `near` resolve against the gazetteer and become a geographic
radius, which is why they find things whose `city` field is missing or spelled
differently. An unresolvable `near` returns an error naming the closest known
places rather than silently dropping the constraint.

## Configuring a region

Everything region-specific lives in one file. Swapping regions means writing
that file and rebuilding:

```bash
make up REGION_FILE=region.vermont.toml
```

```toml
name = "Vermont"                 # required
slug = "vt"                      # required — becomes the MCP server name
admin_level = "state"            # optional
timezone = "America/New_York"    # optional

[bbox]                           # required, or `polygon = [[lat, lng], ...]`
min_lat = 42.726853
min_lng = -73.437740
max_lat = 45.016659
max_lng = -71.464555

[centroid]                       # optional, defaults to the bbox centre
lat = 43.916
lng = -72.669

[vocabulary]                     # optional, defaults to general English
synonyms = [
  ["sugarhouse", "sugar shack", "maple syrup", "sugaring"],
  ["creemee", "soft serve", "ice cream"],
]

[[cities]]                       # optional, but this is the useful part
name = "Stowe"
aliases = ["Stowe, VT"]
lat = 44.4654
lng = -72.6874
county = "Lamoille"
default_radius_m = 12000
```

Everything derives from it: the geo filter on every query, the ingest gate
that drops out-of-region documents, `city`/`near` resolution in the MCP
tools, the Overpass and Wikipedia search grids, Nominatim's bounding viewbox,
and the copy on the dashboard and public form. Startup validation rejects an
inverted bbox or a gazetteer city that falls outside it.

Three things worth knowing:

- **The grid follows the region's area.** Overpass times out on large
  queries, so the bbox is cut into tiles of about 75 km a side — Colorado
  gets 54, Vermont 12. Tune with `OVERPASS_TILE_KM` if queries start timing
  out; the sweep time (tiles × interval) is logged on every run.
- **`[vocabulary]` replaces the defaults, it does not extend them.** Setting
  one synonym group drops the rest, so edit the shipped list rather than
  appending to it. It matters more than it looks: OpenStreetMap tags a winery
  `craft=winery` while people search for a "vineyard".
- **The file is baked into the images at build time**, because heyo's
  `firecracker_containerd` backend takes no `--mount`. `REGION_CONFIG` still
  points elsewhere at runtime wherever a mount is available.

Still English-only: `PLACE_LABELS` in `bot/src/sources/wikipedia.rs` decides
which geotagged articles also become places by matching Wikidata `P31`
labels like `"ski resort"`. Point `WIKIPEDIA_API` at another language and
you will get articles but no places until that list is translated.

## The dashboard

Two surfaces, on two ports, with **no authentication of their own** — the
load balancer in front of the VM owns access control, and the port split is
what makes that a one-line policy.

**Port 8090 — operator.** Document counts and index freshness, per-source
indexer health (runs, last run, written vs. unchanged vs. rejected, last
error), the crawl backlog, the live category values, and the submission
queue. There is also an index browser for finding the document an edit
request is talking about. Put this behind whatever auth the balancer offers.

**Port 8091 — public.** A form where anyone can ask for something to be
added, updated, removed or corrected. No account. They get back an
unguessable 24-character reference and can check its status at any time.
Being an open endpoint, it has a honeypot field, per-IP rate limiting
(`SUBMIT_PER_HOUR`, default 10) and length caps on every field. An email
address is optional, never indexed, and never shown on any public page.

### What happens to a request

Approving one on the dashboard does not edit the index directly — it queues
the work for the indexer, which picks it up within a couple of minutes:

- **Add** — placed by coordinates, else a geocoded address, else the town
  centre, then indexed with a `submitted` tag. If a website was given it also
  goes on the crawl frontier, so the stub is replaced by the real page.
- **Remove** — the document is deleted *and* added to a suppression list.
  Deleting alone is not enough: the source that produced it still has it, so
  the next sweep would put it straight back.
- **Update / Correction** — the website is queued for re-reading. Fields on a
  source-derived document are deliberately never overwritten, because the
  next sweep of that source would simply undo it.

An approved request the indexer cannot act on is **not** retried forever and
is never falsely marked done. It gets an `apply_note` saying exactly what was
missing, drops out of the indexer's queue, and reappears on the dashboard for
the reviewer. Re-approving clears the note and tries again.

## What gets indexed

Three indexes sharing one geo envelope — `places`, `events`, `articles` — fed
by three sources:

- **`overpass`** — OpenStreetMap POIs. The region's bounding box is cut into a
  48-tile grid and one tile is fetched per run, because Overpass times out on a
  whole state. Gives exact coordinates, addresses, phone numbers, opening hours.
- **`wikipedia`** — geotagged articles, typed by Wikidata `P31`. Rather than a
  hardcoded Q-id table, the type entities are resolved to their English labels
  in one extra batched call, so "winery" or "ski resort" appears as a category
  automatically. Articles that are somewhere you can *go* also become places.
- **`crawl`** — a polite crawler over sites you name in `CRAWL_SEEDS`. Prefers
  schema.org JSON-LD (`Event`, `Restaurant`, `NewsArticle`), falls back to
  OpenGraph. Every page it reads is mined for in-domain links, which is how the
  index expands.
- **`submissions`** — approved requests from the public page, above.

Everything funnels through one pipeline that gates on geography, normalises,
deduplicates, and — importantly — compares a content hash against what is
already indexed, so write volume tracks actual change rather than crawl volume.
A document that cannot be placed inside the region is **dropped, not guessed
at**. That is what keeps "everything here is in the region" true.

Add a source by implementing `Source` in `bot/src/sources/` and registering it
in `sources::all()`.

## Configuration

| variable | services | notes |
|---|---|---|
| `PORT`, `BIND_HOST` | mcp, bot | heyo passes `--port`; defaults 8080 / 8081 |
| `REGION_CONFIG` | mcp, bot | default `/etc/regional/region.toml` |
| `MEILI_URL` | mcp, bot | e.g. `http://meilisearch:7700` |
| `MEILI_MASTER_KEY` | bot | admin — the indexer writes |
| `MEILI_SEARCH_KEY` | mcp | **search-only**; see `deploy/create-search-key.sh` |
| `MCP_AUTH_TOKEN` | mcp | required unless `MCP_ALLOW_ANONYMOUS=1` |
| `CONTACT_EMAIL` | bot | **required.** Goes in the User-Agent |
| `CRAWL_SEEDS` | bot | `https://site|Town,https://other` |
| `SOURCE_INTERVALS` | bot | `overpass=20m,wikipedia=15m,crawl=10m` |
| `OVERPASS_TILE_KM` | bot | tile size; the tile count follows the region area |
| `REGION_FILE` | build arg | which region config to bake in |
| `DISABLED_SOURCES` | bot | `crawl,wikipedia` |
| `ADMIN_PORT`, `PUBLIC_PORT` | dashboard | default 8090 / 8091; must differ |
| `BOT_HEALTH_URL` | dashboard | where to read indexer metrics |
| `PUBLIC_TITLE`, `PUBLIC_CONTACT` | dashboard | shown on the public page |
| `SUBMIT_PER_HOUR` | dashboard | per-IP cap on the public form, default 10 |
| `RUST_LOG` | all | default `info` |

Two things worth knowing:

- **`CONTACT_EMAIL` is not optional.** OpenStreetMap, Wikimedia and Nominatim
  all require a real contact in the User-Agent. Overpass answers `406 Not
  Acceptable` to an unidentified client, so the indexer refuses to start
  without one rather than getting quietly blocked an hour in.
- **The MCP server fails closed.** With no `MCP_AUTH_TOKEN` it will not start,
  because the endpoint is internet-facing once deployed. Opt out deliberately
  with `MCP_ALLOW_ANONYMOUS=1`.

## Deploying to heyo

heyo's `firecracker_containerd` backend pulls ordinary OCI references and boots
them as Firecracker microVMs, so the Dockerfiles here *are* the VM image build.

```bash
REGISTRY=ghcr.io/your-org \
MEILI_MASTER_KEY=... MCP_AUTH_TOKEN=... CONTACT_EMAIL=you@example.com \
make deploy
```

Add `REGION_FILE=region.vermont.toml` to bake in a different region. `make
deploy` is a one-line wrapper over `deploy/heyo-deploy.sh`, which you can
equally run directly.

It builds and pushes all four images, then deploys meilisearch (private) →
bot (private) → mcp (public) → dashboard, minting a search-only Meilisearch
key for the MCP service along the way. The dashboard's admin port is bound
`--private` and its public port is bound openly, which is the entire access
policy — point the app-lb's auth at 8090 and leave 8091 alone.

Note on durability: `firecracker_containerd` takes no `--mount`, and heyo's
named volumes are host directories rather than cloud disks, so the database VM
has no attached storage. That is survivable by design — the index is
reconstructible and the indexer rebuilds it continuously — but replacing that
VM costs a full re-index, so `--no-ttl` keeps it from being reaped.

## Development

```bash
make check          # fmt + clippy + tests — run before committing
make bot-once       # one cycle of every source, then exit
```

`make check` runs `cargo fmt --check`, `cargo clippy -D warnings` and
`cargo test --workspace`; each is also a target of its own — `make test`,
`make clippy`, `make fmt`. To run a binary on the host against the database
in Docker:

```bash
make meili          # just Meilisearch, in the background
make run-mcp        # or run-bot, run-bot-once, run-dashboard
```

`make bot-once` is the fastest way to see whether a source change actually
produces documents. The log line to watch is `source cycle complete`, which
reports `received / written / unchanged / out_of_region / invalid` — on a
second run of unchanged upstream data, `written` should be 0.
